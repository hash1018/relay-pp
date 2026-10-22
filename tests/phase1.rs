//! Phase 1 end to end, on the loopback alone: a camera of media-pp's own
//! pulled by relay-pp, played over RTSP and WHEP, its status read over
//! HTTP, and followed across the camera going away and coming back; and
//! channels made and removed over the API.

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
use relay_pp::{ChannelConfig, Config, Server, SourceState, Transport};

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

/// `method path` on `addr` with a JSON `body`, if any: the status, the
/// headers and the body.
fn http(addr: SocketAddr, method: &str, path: &str, body: Option<&str>) -> (u16, String, String) {
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
fn channel(name: &str, source: &str) -> String {
    format!(r#"{{"name":"{name}","source":"{source}"}}"#)
}

/// A server on the loopback, with no channel.
fn server() -> Server {
    let config = Config::parse(
        r#"
        [rtsp]
        listen = "127.0.0.1:0"
        [webrtc]
        listen = "127.0.0.1:0"
        [http]
        listen = "127.0.0.1:0"
        "#,
    )
    .unwrap();
    Server::start(config).expect("start")
}

fn state_of(server: &Server) -> SourceState {
    server.status()[0].state
}

#[test]
fn a_camera_is_pulled_played_and_followed_across_a_drop() {
    let camera = Camera::start("127.0.0.1:0".parse().unwrap());
    let camera_addr = camera.addr;
    let server = server();
    let (status, _, body) = http(
        server.http_addr(),
        "POST",
        "/api/v1/channels",
        Some(&channel(
            "cam1",
            &format!("rtsp://user:secret@{camera_addr}/cam"),
        )),
    );
    assert_eq!(status, 201, "{body}");
    wait_for("the camera live", || state_of(&server) == SourceState::Live);

    let (status, _, body) = http(server.http_addr(), "GET", "/api/v1/channels", None);
    assert_eq!(status, 200, "{body}");
    assert!(
        body.contains(r#""name":"cam1""#) && body.contains(r#""state":"live""#),
        "{body}"
    );
    assert!(
        !body.contains("secret"),
        "no credentials in an answer: {body}"
    );
    let (status, _, _) = http(server.http_addr(), "GET", "/api/v1/channels/nowhere", None);
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
fn channels_are_made_and_removed_over_the_api() {
    let camera = Camera::start("127.0.0.1:0".parse().unwrap());
    let source = format!("rtsp://{}/cam", camera.addr);
    let server = server();
    let api = server.http_addr();

    // What cannot be made is refused, and nothing is left of it.
    for (body, why) in [
        (channel("front door", &source), "a name no URL carries"),
        (channel("cam1", "http://10.0.0.1/"), "a source not RTSP"),
        (r#"{"name":"cam1"}"#.to_owned(), "no source"),
        ("not json".to_owned(), "not JSON"),
    ] {
        let (status, _, answer) = http(api, "POST", "/api/v1/channels", Some(&body));
        assert_eq!(status, 400, "{why}: {answer}");
    }
    assert!(server.status().is_empty());

    let (status, head, body) = http(
        api,
        "POST",
        "/api/v1/channels",
        Some(&channel("cam1", &source)),
    );
    assert_eq!(status, 201, "{body}");
    assert!(
        head.to_ascii_lowercase()
            .contains("location: /api/v1/channels/cam1"),
        "{head}"
    );
    assert!(body.contains(r#""name":"cam1""#), "{body}");
    let (status, _, _) = http(
        api,
        "POST",
        "/api/v1/channels",
        Some(&channel("cam1", &source)),
    );
    assert_eq!(status, 409, "a name is one channel's");

    let (status, _, _) = http(api, "DELETE", "/api/v1/channels/cam1", None);
    assert_eq!(status, 204);
    let (status, _, _) = http(api, "GET", "/api/v1/channels/cam1", None);
    assert_eq!(status, 404);
    let (status, _, _) = http(api, "DELETE", "/api/v1/channels/cam1", None);
    assert_eq!(status, 404);

    // Made again under the same name, it is the one players get: nothing
    // of the one removed takes it off a listener.
    let (status, _, body) = http(
        api,
        "POST",
        "/api/v1/channels",
        Some(&channel("cam1", &source)),
    );
    assert_eq!(status, 201, "{body}");
    wait_for("the camera live", || state_of(&server) == SourceState::Live);
    let (rtsp, over_rtsp) = rtsp_player(&format!("rtsp://{}/cam1", server.rtsp_addr()));
    wait_for("RTSP packets", || over_rtsp.get() > 20);

    rtsp.stop();
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

/// A camera that takes a connection and never answers: opening it waits
/// out the whole timeout.
fn silent_camera() -> std::net::TcpListener {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap()
}

fn pulling(name: &str, camera: SocketAddr) -> ChannelConfig {
    ChannelConfig {
        name: name.to_owned(),
        source: format!("rtsp://{camera}/cam"),
        transport: Transport::Tcp,
    }
}

#[test]
fn removing_a_channel_stuck_opening_holds_up_no_other_add() {
    let silent = silent_camera();
    let server = Arc::new(server());
    server
        .add_channel(&pulling("stuck", silent.local_addr().unwrap()))
        .unwrap();
    // Opening it now waits out the camera's timeout, and so does its
    // removal.
    let removing = std::thread::spawn({
        let server = Arc::clone(&server);
        move || server.remove_channel("stuck")
    });
    wait_for("the stuck channel gone from the list", || {
        server.status().is_empty()
    });
    let began = Instant::now();
    server
        .add_channel(&pulling("other", silent.local_addr().unwrap()))
        .unwrap();
    assert!(
        began.elapsed() < Duration::from_secs(1),
        "an add waited {:?} for a removal",
        began.elapsed()
    );
    // And the name is free again at once.
    server
        .add_channel(&pulling("stuck", silent.local_addr().unwrap()))
        .unwrap();
    assert!(removing.join().unwrap());
    Arc::into_inner(server).unwrap().stop();
}

#[test]
fn stopping_waits_for_every_camera_together() {
    let silent = silent_camera();
    let server = server();
    // Opens begun across a camera's timeout, none of which a stop can cut
    // short: the stop waits for them together, one timeout at most.
    for i in 0..10 {
        server
            .add_channel(&pulling(&format!("cam{i}"), silent.local_addr().unwrap()))
            .unwrap();
        std::thread::sleep(Duration::from_millis(500));
    }
    let began = Instant::now();
    server.stop();
    assert!(
        began.elapsed() < Duration::from_secs(7),
        "stopping took {:?}: longer than one camera's timeout",
        began.elapsed()
    );
}
