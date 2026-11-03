//! Cameras pulled on demand: opened once as the channel is made, connected
//! while something wants them — a player, a request waiting to play, a
//! recording — let go of after `close_after`, and not tried again for
//! nobody.

mod common;

use std::time::{Duration, Instant};

use common::*;
use relay_pp::{Server, SourceState};

/// What `[on_demand]` lets go after, short for the tests' sake.
const CLOSE_AFTER: &str = "[on_demand]\nclose_after = \"2s\"";

fn on_demand(name: &str, source: &str) -> String {
    format!(r#"{{"name":"{name}","source":"{source}","on_demand":true}}"#)
}

fn made(server: &Server, name: &str, source: &str) {
    let (status, _, answer) = http(
        server.http_addr(),
        "POST",
        "/api/v1/channels",
        Some(&on_demand(name, source)),
    );
    assert_eq!(status, 201, "{answer}");
}

fn idle(server: &Server) -> bool {
    state_of(server) == SourceState::Idle
}

fn connections(server: &Server) -> u64 {
    server.status()[0].connections
}

#[test]
fn a_camera_is_connected_only_while_it_is_played() {
    let camera = Camera::start("127.0.0.1:0".parse().unwrap());
    let server = server_with(CLOSE_AFTER);
    made(&server, "cam1", &format!("rtsp://{}/cam", camera.addr));

    // Opened once, to say it answers, and let go of.
    wait_for("idle after the camera answered", || idle(&server));
    let status = &server.status()[0];
    assert!(status.on_demand);
    assert_eq!((status.connections, status.error.as_deref()), (0, None));

    // Asked to play: connected for it.
    let url = play_rtsp(server.http_addr(), "cam1");
    assert_eq!(state_of(&server), SourceState::Live);
    assert_eq!(connections(&server), 1);
    let (rtsp, over_rtsp) = rtsp_player(&url);
    wait_for("RTSP packets", || over_rtsp.get() > 20);

    // Played, it stays connected past `close_after`.
    std::thread::sleep(Duration::from_secs(4));
    assert_eq!(state_of(&server), SourceState::Live);
    assert_eq!(connections(&server), 1);

    // The player gone, it is let go of.
    rtsp.stop();
    let left = Instant::now();
    wait_for("idle after the player left", || idle(&server));
    assert!(
        left.elapsed() >= Duration::from_millis(1500),
        "let go of after {:?}, sooner than close_after",
        left.elapsed()
    );
    assert_eq!(server.status()[0].readers, 0);

    // A player coming straight to the listener, which serves the channel
    // already, has it connected again.
    let (rtsp, over_rtsp) = rtsp_player(&url);
    wait_for("RTSP packets, from idle", || over_rtsp.get() > 20);
    assert_eq!(connections(&server), 2);
    rtsp.stop();
    wait_for("idle again", || idle(&server));

    // And a WebRTC player, through WHEP, likewise.
    let (whep, over_webrtc) = whep_player(&format!("http://{}/cam1/whep", server.http_addr()));
    wait_for("WebRTC packets", || over_webrtc.get() > 20);
    assert_eq!(connections(&server), 3);
    whep.stop();
    wait_for("idle after WebRTC", || idle(&server));

    // And HLS, its playlist asked for.
    let (status, _, playlist) = http_bytes(server.http_addr(), "/cam1/index.m3u8");
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&playlist));
    assert_eq!(state_of(&server), SourceState::Live);
    assert_eq!(connections(&server), 4);

    server.stop();
    camera.stop();
}

#[test]
fn a_recording_keeps_the_camera_connected() {
    let camera = Camera::start("127.0.0.1:0".parse().unwrap());
    let root = scratch("on_demand_record");
    let server = server_with(&format!(
        "{CLOSE_AFTER}\n[record]\ndir = {:?}",
        root.display().to_string()
    ));
    made(&server, "cam1", &format!("rtsp://{}/cam", camera.addr));
    wait_for("idle", || idle(&server));

    let record = |method| {
        http(
            server.http_addr(),
            method,
            "/api/v1/channels/cam1/record",
            None,
        )
        .0
    };
    assert_eq!(record("POST"), 201);
    wait_for("live for the recording", || {
        state_of(&server) == SourceState::Live
    });
    std::thread::sleep(Duration::from_secs(4));
    assert_eq!(state_of(&server), SourceState::Live);
    assert_eq!(connections(&server), 1, "held, not connected again");

    assert_eq!(record("DELETE"), 204);
    wait_for("idle after the recording", || idle(&server));

    server.stop();
    camera.stop();
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn a_camera_that_does_not_answer_is_not_tried_for_nobody() {
    // A port nothing listens on: refused at once.
    let closed = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let server = server_with(CLOSE_AFTER);
    made(&server, "dark", &format!("rtsp://{closed}/cam"));

    // Made, it says at once that the camera does not answer.
    wait_for("idle with why", || {
        idle(&server) && server.status()[0].error.is_some()
    });

    // A player waits for it, is told why it cannot, and it is tried no
    // more once nothing waits.
    let (status, _, answer) = http(
        server.http_addr(),
        "POST",
        "/api/v1/channels/dark/play",
        Some(r#"{"protocol":"rtsp"}"#),
    );
    assert_eq!(status, 503, "{answer}");
    assert!(answer.contains("dark"), "{answer}");
    wait_for("idle again", || idle(&server));
    let error = server.status()[0].error.clone();
    assert!(error.is_some(), "it still says why");
    std::thread::sleep(Duration::from_secs(4));
    assert!(idle(&server), "not tried again for nobody");
    assert_eq!(connections(&server), 0);

    server.stop();
}

#[test]
fn only_a_camera_pulled_is_on_demand() {
    let server = server();
    let (status, _, answer) = http(
        server.http_addr(),
        "POST",
        "/api/v1/channels",
        Some(r#"{"name":"pushed","publish":"rtmp","on_demand":true}"#),
    );
    assert_eq!(status, 400, "{answer}");
    assert!(answer.contains("on demand"), "{answer}");
    assert!(server.status().is_empty());
    server.stop();
}
