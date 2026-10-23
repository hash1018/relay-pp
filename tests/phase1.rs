//! Phase 1 end to end, on the loopback alone: a camera of media-pp's own
//! pulled by relay-pp, played over RTSP and WHEP, its status read over
//! HTTP, and followed across the camera going away and coming back; and
//! channels made and removed over the API.

mod common;

use std::time::{Duration, Instant};

use common::*;
use relay_pp::{Config, Server, SourceState};

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

    // Made with its source alone: no listener has it until a player asks.
    assert!(server.status()[0].sinks.is_empty());
    let direct = format!("rtsp://{}/cam1", server.rtsp_addr());
    assert!(!rtsp_opens(&direct), "not on the RTSP listener yet");
    let url = play_rtsp(server.http_addr(), "cam1");
    assert_eq!(
        url, direct,
        "the host the request came to, and the RTSP port"
    );
    let (rtsp, over_rtsp) = rtsp_player(&url);
    let (whep, over_webrtc) = whep_player(&format!("http://{}/cam1/whep", server.http_addr()));
    wait_for("RTSP packets", || over_rtsp.get() > 20);
    wait_for("WebRTC packets", || over_webrtc.get() > 20);
    assert_eq!(server.status()[0].sinks, ["rtsp", "webrtc"]);
    assert_eq!(
        server.status()[0].readers,
        2,
        "both players read the broadcast"
    );
    assert_eq!(
        play_rtsp(server.http_addr(), "cam1"),
        url,
        "asked again, the same"
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
    let (rtsp, over_rtsp) = rtsp_player(&play_rtsp(api, "cam1"));
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
        [rtmp]
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

#[test]
fn removing_a_channel_stuck_opening_waits_for_no_open_and_holds_up_no_add() {
    let silent = silent_camera();
    let server = server();
    server
        .add_channel(&pulling("stuck", silent.local_addr().unwrap()))
        .unwrap();
    // Its open waits out the camera's timeout; the removal lets go of it.
    std::thread::sleep(Duration::from_millis(200));
    let began = Instant::now();
    assert!(server.remove_channel("stuck"));
    assert!(
        began.elapsed() < Duration::from_secs(1),
        "a removal waited {:?} for an open",
        began.elapsed()
    );
    // And the name is free again at once, the other adds unheld.
    let began = Instant::now();
    for name in ["stuck", "other"] {
        server
            .add_channel(&pulling(name, silent.local_addr().unwrap()))
            .unwrap();
    }
    assert!(
        began.elapsed() < Duration::from_secs(1),
        "adds waited {:?}",
        began.elapsed()
    );
    server.stop();
}

#[test]
fn stopping_waits_for_no_camera_opening() {
    let silent = silent_camera();
    let server = server();
    // Opens begun across a camera's timeout, none of which a stop waits
    // out: the runtime's own shutdown gives what is left of them 2 seconds.
    for i in 0..10 {
        server
            .add_channel(&pulling(&format!("cam{i}"), silent.local_addr().unwrap()))
            .unwrap();
        std::thread::sleep(Duration::from_millis(500));
    }
    let began = Instant::now();
    server.stop();
    assert!(
        began.elapsed() < Duration::from_secs(4),
        "stopping took {:?}",
        began.elapsed()
    );
}

#[test]
fn playing_waits_for_the_camera_and_says_why_it_cannot() {
    let silent = silent_camera();
    let server = server();
    let api = server.http_addr();
    server
        .add_channel(&pulling("dark", silent.local_addr().unwrap()))
        .unwrap();
    let play = |name: &str, body: &str| {
        http(
            api,
            "POST",
            &format!("/api/v1/channels/{name}/play"),
            Some(body),
        )
    };
    let (status, _, _) = play("nowhere", r#"{"protocol":"rtsp"}"#);
    assert_eq!(status, 404);
    for body in [r#"{"protocol":"webrtc"}"#, r#"{"protocol":"hls"}"#, "{}"] {
        let (status, _, answer) = play("dark", body);
        assert_eq!(status, 400, "{body}: {answer}");
    }
    // A camera that never answers: the request waits for it, then says so.
    let began = Instant::now();
    let (status, _, answer) = play("dark", r#"{"protocol":"rtsp"}"#);
    assert_eq!(status, 503, "{answer}");
    assert!(answer.contains("dark"), "{answer}");
    assert!(
        began.elapsed() >= Duration::from_secs(9),
        "waited {:?}",
        began.elapsed()
    );
    server.stop();
}
