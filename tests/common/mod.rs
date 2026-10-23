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
use relay_pp::{ChannelConfig, Client, Config, Server, SourceState, Transport};

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
        Self::started(addr, false, false).expect("OpenH264 is always there")
    }

    /// One with sound as well — AAC — and so two tracks, which this
    /// camera, media-pp's RTSP server, describes with no `rtptime`.
    pub fn with_sound(addr: SocketAddr) -> Self {
        Self::started(addr, true, false).expect("OpenH264 is always there")
    }

    /// One whose pictures have B-frames — sent out of the order they are
    /// shown in — by `libx264`: `None` where this FFmpeg has none.
    pub fn with_b_frames(addr: SocketAddr) -> Option<Self> {
        Self::started(addr, false, true)
    }

    fn started(addr: SocketAddr, sound: bool, b_frames: bool) -> Option<Self> {
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
                codec: if b_frames {
                    VideoCodec::H264
                } else {
                    VideoCodec::OpenH264
                },
                width: 320,
                height: 240,
                pixel_format: ffmpeg::format::Pixel::YUV420P,
                frame_rate: ffmpeg::Rational::new(15, 1),
                bit_rate: 300_000,
                gop_size: 15,
                max_b_frames: b_frames.then_some(2),
            },
        )
        .ok()?;
        let sound_encoder = sound.then(|| {
            SwAudioEncoder::new(
                "sound-encoder",
                SwAudioEncoderOptions {
                    codec: AudioCodec::Aac,
                    sample_rate: 48_000,
                    channels: 1,
                    bit_rate: 64_000,
                },
            )
            .expect("AAC is always there")
        });
        let mut tracks = vec![StreamInfo::from(&encoder)];
        if let Some(sound_encoder) = &sound_encoder {
            tracks.push(StreamInfo::from(sound_encoder));
        }
        let mut sinks = broadcast.publish(&tracks).unwrap();
        let sound_sink = sound.then(|| sinks.remove(1));
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
    let mut stream = TcpStream::connect(addr).unwrap();
    let body = body.unwrap_or_default();
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\
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
        source: format!("rtsp://{camera}/cam"),
        transport: Transport::Tcp,
        client: Client::Auto,
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
