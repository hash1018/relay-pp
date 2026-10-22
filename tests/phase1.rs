//! Phase 1 end to end, on the loopback alone: a camera of media-pp's own
//! pulled by relay-pp, played over RTSP and WHEP, its status read over
//! HTTP, and followed across the camera going away and coming back.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

use media_pp::driver::DriverRunner;
use media_pp::elements::{
    Broadcast, BroadcastOptions, CounterHandle, PacketCounter, RtspOptions, RtspPathMode,
    RtspServer, RtspServerOptions, RtspSource, RtspTransport, StreamInfo, SwEncoder,
    SwEncoderOptions, TestVideoOptions, TestVideoSource, VideoCodec, WhepOptions, WhepSource,
};
use media_pp::ffmpeg;
use media_pp::pipeline::Pipeline;
use relay_pp::{Config, Server, SourceState};

const WITHIN: Duration = Duration::from_secs(20);

/// An RTSP camera: a picture of media-pp's test source, encoded live and
/// served at `rtsp://{addr}/cam`.
struct Camera {
    server: Arc<DriverRunner>,
    input: Arc<Pipeline>,
    addr: SocketAddr,
}

impl Camera {
    fn start(addr: SocketAddr) -> Self {
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
                codec: VideoCodec::OpenH264,
                width: 320,
                height: 240,
                pixel_format: ffmpeg::format::Pixel::YUV420P,
                frame_rate: ffmpeg::Rational::new(15, 1),
                bit_rate: 300_000,
                gop_size: 15,
                max_b_frames: None,
            },
        )
        .expect("OpenH264 is always there");
        let sink = broadcast
            .publish(&[StreamInfo::from(&encoder)])
            .unwrap()
            .remove(0);
        let pictures = TestVideoSource::new(
            "pictures",
            TestVideoOptions {
                width: 320,
                height: 240,
                frame_rate: ffmpeg::Rational::new(15, 1),
            },
        );
        let (input, ()) = Pipeline::new("camera-input", pictures, |source, ctx| {
            let branch = ctx.branch().queue("encode", 4).pipe(encoder).to(sink)?;
            ctx.attach(source, 0, branch)?;
            Ok(())
        })
        .unwrap();
        input.run().unwrap();
        Self {
            addr: handle.local_addr(),
            server,
            input,
        }
    }

    fn stop(self) {
        self.input.stop();
        self.server.stop();
    }
}

/// A player of `url`'s first stream over `transport`: how many packets it
/// has been handed.
fn rtsp_player(url: &str) -> (Arc<Pipeline>, CounterHandle) {
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

fn whep_player(url: &str) -> (Arc<Pipeline>, CounterHandle) {
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

fn wait_for(what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + WITHIN;
    while !done() {
        assert!(Instant::now() < deadline, "{what} within {WITHIN:?}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// `GET path` on `addr`: the status and the body.
fn http_get(addr: SocketAddr, path: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).unwrap();
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let status = response
        .split(' ')
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    let body = response
        .split_once("\r\n\r\n")
        .map_or(String::new(), |(_, body)| body.to_owned());
    (status, body)
}

fn state_of(server: &Server) -> SourceState {
    server.status()[0].state
}

#[test]
fn a_camera_is_pulled_played_and_followed_across_a_drop() {
    let camera = Camera::start("127.0.0.1:0".parse().unwrap());
    let camera_addr = camera.addr;
    let config = Config::parse(&format!(
        r#"
        [rtsp]
        listen = "127.0.0.1:0"
        [webrtc]
        listen = "127.0.0.1:0"
        [http]
        listen = "127.0.0.1:0"
        [channels.cam1]
        source = "rtsp://user:secret@{camera_addr}/cam"
        "#
    ))
    .unwrap();
    let server = Server::start(config).expect("start");
    wait_for("the camera live", || state_of(&server) == SourceState::Live);

    let (status, body) = http_get(server.http_addr(), "/api/v1/channels");
    assert_eq!(status, 200, "{body}");
    assert!(
        body.contains(r#""name":"cam1""#) && body.contains(r#""state":"live""#),
        "{body}"
    );
    assert!(
        !body.contains("secret"),
        "no credentials in an answer: {body}"
    );
    let (status, _) = http_get(server.http_addr(), "/api/v1/channels/nowhere");
    assert_eq!(status, 404);

    let (rtsp, over_rtsp) = rtsp_player(&format!("rtsp://{}/cam1", server.rtsp_addr()));
    let (whep, over_webrtc) = whep_player(&format!("http://{}/cam1/whep", server.http_addr()));
    wait_for("RTSP packets", || over_rtsp.get() > 20);
    wait_for("WebRTC packets", || over_webrtc.get() > 20);
    assert_eq!(
        server.status()[0].readers,
        2,
        "both players read the broadcast"
    );

    // The camera goes away: the channel says why, and keeps its players.
    camera.stop();
    wait_for("the drop noticed", || {
        state_of(&server) != SourceState::Live
    });
    wait_for("the reason kept", || server.status()[0].error.is_some());

    // It comes back where it was: the channel connects again by itself, and
    // the RTSP player, connected throughout, goes on.
    let camera = Camera::start(camera_addr);
    wait_for("the camera live again", || {
        let status = &server.status()[0];
        status.state == SourceState::Live && status.connections == 2
    });
    let before = over_rtsp.get();
    wait_for("RTSP packets after the camera came back", || {
        over_rtsp.get() > before + 20
    });

    rtsp.stop();
    whep.stop();
    server.stop();
    camera.stop();
}

#[test]
fn a_listener_that_cannot_bind_stops_the_start_saying_which() {
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let config = Config::parse(&format!(
        r#"
        [rtsp]
        listen = "127.0.0.1:0"
        [webrtc]
        listen = "127.0.0.1:0"
        [http]
        listen = "{}"
        "#,
        taken.local_addr().unwrap()
    ))
    .unwrap();
    let error = Server::start(config)
        .err()
        .expect("the port is taken")
        .to_string();
    assert!(error.contains("http.listen"), "{error}");
}
