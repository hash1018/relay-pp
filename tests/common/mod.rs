//! What the end-to-end tests share: a camera of media-pp's own, players,
//! a server on the loopback and plain HTTP to it.

#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

use media_pp::driver::DriverRunner;
use media_pp::elements::{
    AudioCodec, Broadcast, BroadcastOptions, CounterHandle, PacketCounter, RtspOptions,
    RtspPathMode, RtspServer, RtspServerOptions, RtspSource, RtspTransport, StreamInfo,
    SwAudioEncoder, SwAudioEncoderOptions, SwEncoder, SwEncoderOptions, TestAudioOptions,
    TestAudioSource, TestVideoOptions, TestVideoSource, VideoCodec, WhepOptions, WhepSource,
};
use media_pp::ffmpeg;
use media_pp::pipeline::{Pipeline, PipelineBuilder};
use relay_pp::{ChannelConfig, Config, ConvertChoice, Server, SourceState, Transport};

pub const WITHIN: Duration = Duration::from_secs(20);

/// An RTSP camera: a picture of media-pp's test source, encoded live and
/// served at `rtsp://{addr}/cam`.
pub struct Camera {
    server: Arc<DriverRunner>,
    pub input: Arc<Pipeline>,
    pub addr: SocketAddr,
}

impl Camera {
    pub fn start(addr: SocketAddr) -> Self {
        Self::started(addr, VideoCodec::OpenH264, None, false).expect("OpenH264 is always there")
    }

    /// One with sound as well — AAC — and so two tracks, which this
    /// camera, media-pp's RTSP server, describes with no `rtptime`.
    pub fn with_sound(addr: SocketAddr) -> Self {
        Self::started(addr, VideoCodec::OpenH264, Some(AudioCodec::Aac), false)
            .expect("OpenH264 is always there")
    }

    /// One whose pictures have B-frames — sent out of the order they are
    /// shown in — by `libx264`: `None` where this FFmpeg has none.
    pub fn with_b_frames(addr: SocketAddr) -> Option<Self> {
        Self::started(addr, VideoCodec::H264, None, true)
    }

    /// One with Opus sound, by `libopus`: `None` where this FFmpeg has none.
    pub fn with_opus(addr: SocketAddr) -> Option<Self> {
        Self::started(addr, VideoCodec::OpenH264, Some(AudioCodec::Opus), false)
    }

    /// One whose pictures are in `codec`, and with AAC sound where `sound`:
    /// `None` where this FFmpeg has no encoder of it.
    pub fn with_pictures(addr: SocketAddr, codec: VideoCodec, sound: bool) -> Option<Self> {
        Self::started(addr, codec, sound.then_some(AudioCodec::Aac), false)
    }

    fn started(
        addr: SocketAddr,
        pictures: VideoCodec,
        sound: Option<AudioCodec>,
        b_frames: bool,
    ) -> Option<Self> {
        let (server, handle) = RtspServer::bind("camera", addr, RtspServerOptions::default())
            .expect("bind the camera");
        let broadcast = Broadcast::new("cam", BroadcastOptions::default());
        handle
            .add_path("cam", &broadcast, RtspPathMode::Play)
            .unwrap();
        let server = DriverRunner::new(server);
        server.run().unwrap();
        let encoder = SwEncoder::new(
            "encoder",
            SwEncoderOptions {
                codec: pictures,
                width: 320,
                height: 240,
                pixel_format: ffmpeg::format::Pixel::YUV420P,
                frame_rate: ffmpeg::Rational::new(15, 1),
                bit_rate: 300_000,
                gop_size: 15,
                max_b_frames: Some(if b_frames { 2 } else { 0 }),
            },
        )
        .ok()?;
        let sound_encoder = match sound {
            Some(codec) => Some(
                SwAudioEncoder::new(
                    "sound-encoder",
                    SwAudioEncoderOptions {
                        codec,
                        sample_rate: 48_000,
                        channels: 1,
                        bit_rate: 64_000,
                    },
                )
                .ok()?,
            ),
            None => None,
        };
        let mut tracks = vec![StreamInfo::from(&encoder)];
        if let Some(sound_encoder) = &sound_encoder {
            tracks.push(StreamInfo::from(sound_encoder));
        }
        let mut sinks = broadcast.publish(&tracks).unwrap();
        let sound_sink = sound.map(|_| sinks.remove(1));
        let sink = sinks.remove(0);
        let pictures = TestVideoSource::new(
            "pictures",
            TestVideoOptions {
                width: 320,
                height: 240,
                frame_rate: ffmpeg::Rational::new(15, 1),
            },
        );
        let (builder, ()) = PipelineBuilder::new("camera-input")
            .add_source(pictures, |source, ctx| {
                let branch = ctx.branch().queue("encode", 4).pipe(encoder).to(sink)?;
                ctx.attach(source, 0, branch)?;
                Ok(())
            })
            .unwrap();
        let builder = match (sound_encoder, sound_sink) {
            (Some(sound_encoder), Some(sound_sink)) => {
                let tone = TestAudioSource::new(
                    "tone",
                    TestAudioOptions {
                        sample_rate: 48_000,
                        channels: 1,
                        ..TestAudioOptions::default()
                    },
                );
                builder
                    .add_source(tone, |source, ctx| {
                        let branch = ctx
                            .branch()
                            .queue("encode-sound", 8)
                            .pipe(sound_encoder)
                            .to(sound_sink)?;
                        ctx.attach(source, 0, branch)?;
                        Ok(())
                    })
                    .unwrap()
                    .0
            }
            _ => builder,
        };
        let input = builder.build();
        input.run().unwrap();
        Some(Self {
            addr: handle.local_addr(),
            server,
            input,
        })
    }

    pub fn stop(self) {
        self.input.stop();
        self.server.stop();
    }
}

/// A player of `url`'s first stream over `transport`: how many packets it
/// has been handed.
pub fn rtsp_player(url: &str) -> (Arc<Pipeline>, CounterHandle) {
    let (source, streams) = RtspSource::open(
        "player",
        url,
        RtspOptions {
            transport: RtspTransport::Tcp,
            timeout: Duration::from_secs(5),
        },
    )
    .unwrap_or_else(|error| panic!("{url} opens: {error}"));
    let (counter, counted) = PacketCounter::new("packets");
    let (pipeline, ()) = Pipeline::new("rtsp-player", source, |source, ctx| {
        ctx.attach(source, streams[0].index(), ctx.branch().to(counter)?)?;
        Ok(())
    })
    .unwrap();
    pipeline.run().unwrap();
    (pipeline, counted)
}

pub fn whep_player(url: &str) -> (Arc<Pipeline>, CounterHandle) {
    let (source, streams) = WhepSource::open("player", url, WhepOptions::default())
        .unwrap_or_else(|error| panic!("{url} opens: {error}"));
    let (counter, counted) = PacketCounter::new("packets");
    let (pipeline, ()) = Pipeline::new("whep-player", source, |source, ctx| {
        ctx.attach(source, streams[0].index(), ctx.branch().to(counter)?)?;
        Ok(())
    })
    .unwrap();
    pipeline.run().unwrap();
    (pipeline, counted)
}

pub fn wait_for(what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + WITHIN;
    while !done() {
        assert!(Instant::now() < deadline, "{what} within {WITHIN:?}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// `method path` on `addr` with a JSON `body`, if any: the status, the
/// headers and the body.
pub fn http(
    addr: SocketAddr,
    method: &str,
    path: &str,
    body: Option<&str>,
) -> (u16, String, String) {
    http_as(addr, method, path, body, None)
}

/// [`http`] with `authorization` as the request's `Authorization`, where
/// it is given.
pub fn http_as(
    addr: SocketAddr,
    method: &str,
    path: &str,
    body: Option<&str>,
    authorization: Option<&str>,
) -> (u16, String, String) {
    let mut stream = TcpStream::connect(addr).unwrap();
    let body = body.unwrap_or_default();
    let authorization = authorization
        .map(|value| format!("Authorization: {value}\r\n"))
        .unwrap_or_default();
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n{authorization}\
         Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let status = response
        .split(' ')
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    let (head, body) = response
        .split_once("\r\n\r\n")
        .map_or((response.as_str(), ""), |(head, body)| (head, body));
    (status, head.to_owned(), body.to_owned())
}

/// The body that makes a channel `name` pulling `source`.
pub fn channel(name: &str, source: &str) -> String {
    format!(r#"{{"name":"{name}","source":"{source}"}}"#)
}

/// A server on the loopback, with no channel.
pub fn server() -> Server {
    server_with("")
}

/// A server on the loopback, with no channel, and `more` of its
/// configuration — a `[record]` section, say.
pub fn server_with(more: &str) -> Server {
    let config = Config::parse(&format!(
        r#"
        [rtsp]
        listen = "127.0.0.1:0"
        [webrtc]
        listen = "127.0.0.1:0"
        [http]
        listen = "127.0.0.1:0"
        [rtmp]
        listen = "127.0.0.1:0"
        {more}
        "#
    ))
    .unwrap();
    Server::start(config).expect("start")
}

/// A directory of the test's own under the system's temporary one, empty.
pub fn scratch(label: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "relay_pp_{label}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// `GET path` on `addr` for a body that may be anything but text — a
/// segment: the status, the headers, and the body.
pub fn http_bytes(addr: SocketAddr, path: &str) -> (u16, String, Vec<u8>) {
    let mut stream = TcpStream::connect(addr).unwrap();
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).unwrap();
    let split = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map_or(response.len(), |at| at + 4);
    let head = String::from_utf8_lossy(&response[..split]).into_owned();
    let status = head
        .split(' ')
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    (status, head, response[split..].to_vec())
}

/// `GET path` on `addr` with a `Range` header: the status and the body's
/// length.
pub fn http_range(addr: SocketAddr, path: &str, range: &str) -> (u16, usize) {
    let mut stream = TcpStream::connect(addr).unwrap();
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nRange: bytes={range}\r\n\r\n"
    )
    .unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).unwrap();
    let text = String::from_utf8_lossy(&response);
    let status = text
        .split(' ')
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    let body = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map_or(0, |at| response.len() - at - 4);
    (status, body)
}

pub fn state_of(server: &Server) -> SourceState {
    server.status()[0].state
}

/// A camera that takes a connection and never answers: opening it waits
/// out the whole timeout.
pub fn silent_camera() -> std::net::TcpListener {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap()
}

pub fn pulling(name: &str, camera: SocketAddr) -> ChannelConfig {
    ChannelConfig {
        name: name.to_owned(),
        source: Some(format!("rtsp://{camera}/cam")),
        publish: None,
        transport: Transport::Tcp,
        convert: ConvertChoice::default(),
    }
}

/// `POST /api/v1/channels/{name}/play` for RTSP on `api`: the URL it
/// answers with.
pub fn play_rtsp(api: SocketAddr, name: &str) -> String {
    let (status, _, body) = http(
        api,
        "POST",
        &format!("/api/v1/channels/{name}/play"),
        Some(r#"{"protocol":"rtsp"}"#),
    );
    assert_eq!(status, 200, "{body}");
    let value: serde_json::Value = serde_json::from_str(&body).unwrap();
    value["url"].as_str().unwrap().to_owned()
}

/// Whether an RTSP player opens `url` at all.
pub fn rtsp_opens(url: &str) -> bool {
    RtspSource::open(
        "probe",
        url,
        RtspOptions {
            transport: RtspTransport::Tcp,
            timeout: Duration::from_secs(5),
        },
    )
    .is_ok()
}

/// A WHEP player offering pictures and sound: each track it was given, by
/// codec, and how many packets of it have come. It waits longer than a
/// player's default for the first: pictures converted in software, where
/// no GPU is, come out of x264 seconds after they went in.
pub fn whep_tracks(url: &str) -> (Arc<Pipeline>, Vec<(ffmpeg::codec::Id, CounterHandle)>) {
    let options = WhepOptions {
        describe_within: Duration::from_secs(15),
        ..WhepOptions::default()
    };
    let (source, streams) = WhepSource::open("player", url, options)
        .unwrap_or_else(|error| panic!("{url} opens: {error}"));
    let mut tracks = Vec::new();
    let mut counters = Vec::new();
    for stream in &streams {
        let (counter, counted) = PacketCounter::new(format!("packets-{}", stream.index()));
        tracks.push((stream.parameters().id(), counted));
        counters.push((stream.index(), counter));
    }
    let (pipeline, ()) = Pipeline::new("whep-player", source, |source, ctx| {
        for (index, counter) in counters {
            ctx.attach(source, index, ctx.branch().to(counter)?)?;
        }
        Ok(())
    })
    .unwrap();
    pipeline.run().unwrap();
    (pipeline, tracks)
}

/// The codecs of what an RTSP player of `url` is described.
pub fn rtsp_codecs(url: &str) -> Vec<ffmpeg::codec::Id> {
    let (_, streams) = RtspSource::open(
        "probe",
        url,
        RtspOptions {
            transport: RtspTransport::Tcp,
            timeout: Duration::from_secs(5),
        },
    )
    .unwrap_or_else(|error| panic!("{url} opens: {error}"));
    streams
        .iter()
        .map(|stream| stream.parameters().id())
        .collect()
}

/// How a test's publisher pushes its stream.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Push {
    Rtmp,
    Rtsp,
    Whip,
}

/// A publisher pushing to `url` over `over`, as OBS or an encoder would:
/// pictures by OpenH264 and sound by AAC — Opus over WHIP, which carries no
/// AAC — at their own pace. `token` goes as WHIP's Bearer token. `None`
/// where this FFmpeg has no Opus encoder for WHIP; `Err` where the server
/// refuses the publisher.
pub fn push(
    url: &str,
    over: Push,
    token: Option<String>,
) -> Option<media_pp::Result<Arc<Pipeline>>> {
    use media_pp::elements::{RtmpMuxer, RtspMuxer, WhipOptions, WhipPublisher};
    let pictures_encoder = SwEncoder::new(
        "push-encoder",
        SwEncoderOptions {
            codec: VideoCodec::OpenH264,
            width: 320,
            height: 240,
            pixel_format: ffmpeg::format::Pixel::YUV420P,
            frame_rate: ffmpeg::Rational::new(15, 1),
            bit_rate: 300_000,
            gop_size: 15,
            max_b_frames: Some(0),
        },
    )
    .expect("OpenH264 is always there");
    let sound_encoder = SwAudioEncoder::new(
        "push-sound-encoder",
        SwAudioEncoderOptions {
            codec: if over == Push::Whip {
                AudioCodec::Opus
            } else {
                AudioCodec::Aac
            },
            sample_rate: 48_000,
            channels: 1,
            bit_rate: 64_000,
        },
    )
    .ok()?;
    let pictures = StreamInfo::from(&pictures_encoder);
    let sound = StreamInfo::from(&sound_encoder);
    let made = (|| -> media_pp::Result<Arc<Pipeline>> {
        match over {
            Push::Rtmp => {
                let mut muxer = RtmpMuxer::create(url)?;
                let (a, b) = (
                    muxer.add_stream("pictures", pictures.clone())?,
                    muxer.add_stream("sound", sound.clone())?,
                );
                let mut sinks = muxer.open()?;
                pushing(
                    pictures_encoder,
                    sinks.take(a)?,
                    sound_encoder,
                    sinks.take(b)?,
                )
            }
            Push::Rtsp => {
                let mut muxer = RtspMuxer::create(
                    url,
                    RtspOptions {
                        transport: RtspTransport::Tcp,
                        timeout: Duration::from_secs(5),
                    },
                )?;
                let (a, b) = (
                    muxer.add_stream("pictures", pictures.clone())?,
                    muxer.add_stream("sound", sound.clone())?,
                );
                let mut sinks = muxer.open()?;
                pushing(
                    pictures_encoder,
                    sinks.take(a)?,
                    sound_encoder,
                    sinks.take(b)?,
                )
            }
            Push::Whip => {
                let mut publisher = WhipPublisher::create(
                    "publisher",
                    url,
                    WhipOptions {
                        bearer_token: token,
                        ..WhipOptions::default()
                    },
                )?;
                let a = publisher.add_track(pictures.parameters())?;
                let b = publisher.add_track(sound.parameters())?;
                let mut sinks = publisher.open()?;
                pushing(
                    pictures_encoder,
                    sinks.take(a)?,
                    sound_encoder,
                    sinks.take(b)?,
                )
            }
        }
    })();
    Some(made)
}

/// A pipeline of the test's own pictures and tone, encoded and written into
/// `pictures_sink` and `sound_sink`.
fn pushing<A, B, MA, MB>(
    pictures_encoder: SwEncoder,
    pictures_sink: A,
    sound_encoder: SwAudioEncoder,
    sound_sink: B,
) -> media_pp::Result<Arc<Pipeline>>
where
    A: media_pp::element::IntoTerminal<MA>,
    B: media_pp::element::IntoTerminal<MB>,
{
    let pictures = TestVideoSource::new(
        "push-pictures",
        TestVideoOptions {
            width: 320,
            height: 240,
            frame_rate: ffmpeg::Rational::new(15, 1),
        },
    );
    let tone = TestAudioSource::new(
        "push-tone",
        TestAudioOptions {
            sample_rate: 48_000,
            channels: 1,
            ..TestAudioOptions::default()
        },
    );
    let (builder, ()) = PipelineBuilder::new("publisher").add_source(pictures, |source, ctx| {
        let branch = ctx
            .branch()
            .queue("push-encode", 4)
            .pipe(pictures_encoder)
            .to(pictures_sink)?;
        ctx.attach(source, 0, branch)?;
        Ok(())
    })?;
    let (builder, ()) = builder.add_source(tone, |source, ctx| {
        let branch = ctx
            .branch()
            .queue("push-encode-sound", 8)
            .pipe(sound_encoder)
            .to(sound_sink)?;
        ctx.attach(source, 0, branch)?;
        Ok(())
    })?;
    let pipeline = builder.build();
    pipeline.run()?;
    Ok(pipeline)
}
