//! A camera pulled with media-pp's RTSP client: played over RTSP and WHEP,
//! and followed across a drop, over TCP and over UDP; a camera of two
//! tracks relayed with both; and one sending B-frames relayed in order.

mod common;

use std::net::SocketAddr;
use std::time::Duration;

use common::*;
use media_pp::elements::{RtspOptions, RtspSource, RtspTransport};
use relay_pp::SourceState;

/// Makes `name` pulling the camera at `camera` over `transport`, and waits
/// for it to go live.
fn made(server: &relay_pp::Server, name: &str, camera: SocketAddr, transport: &str) {
    let body =
        format!(r#"{{"name":"{name}","source":"rtsp://{camera}/cam","transport":"{transport}"}}"#);
    let (status, _, answer) = http(server.http_addr(), "POST", "/api/v1/channels", Some(&body));
    assert_eq!(status, 201, "{answer}");
    wait_for("the camera live", || state_of(server) == SourceState::Live);
}

/// The channel played over RTSP and WHEP, and followed across the camera
/// dropping and coming back.
fn played_and_followed(transport: &str) {
    let camera = Camera::start("127.0.0.1:0".parse().unwrap());
    let camera_addr = camera.addr;
    let server = server();
    made(&server, "cam1", camera_addr, transport);
    assert_eq!(server.status()[0].client, Some("rtsp"));
    let (_, _, listed) = http(server.http_addr(), "GET", "/api/v1/channels/cam1", None);
    assert!(listed.contains(r#""client":"rtsp""#), "{listed}");

    let (rtsp, over_rtsp) = rtsp_player(&play_rtsp(server.http_addr(), "cam1"));
    let (whep, over_webrtc) = whep_player(&format!("http://{}/cam1/whep", server.http_addr()));
    wait_for("RTSP packets", || over_rtsp.get() > 20);
    wait_for("WebRTC packets", || over_webrtc.get() > 20);

    camera.stop();
    wait_for("the drop noticed", || {
        state_of(&server) != SourceState::Live
    });
    let camera = Camera::start(camera_addr);
    wait_for("the camera live again", || {
        let status = &server.status()[0];
        status.state == SourceState::Live && status.connections == 2
    });
    let before = over_rtsp.get();
    wait_for("RTSP packets after the camera came back", || {
        over_rtsp.get() > before + 20
    });
    let before = over_webrtc.get();
    wait_for("WebRTC packets after the camera came back", || {
        over_webrtc.get() > before + 20
    });

    rtsp.stop();
    whep.stop();
    server.stop();
    camera.stop();
}

#[test]
fn a_camera_pulled_over_tcp_is_played_and_followed() {
    played_and_followed("tcp");
}

#[test]
fn a_camera_pulled_over_udp_is_played_and_followed() {
    played_and_followed("udp");
}

#[test]
fn a_camera_of_two_tracks_is_relayed_with_both() {
    let camera = Camera::with_sound("127.0.0.1:0".parse().unwrap());
    let server = server();
    made(&server, "cam1", camera.addr, "tcp");

    let url = play_rtsp(server.http_addr(), "cam1");
    let (_, streams) = RtspSource::open(
        "probe",
        &url,
        RtspOptions {
            transport: RtspTransport::Tcp,
            timeout: Duration::from_secs(5),
        },
    )
    .unwrap();
    assert_eq!(streams.len(), 2, "pictures and sound both relayed");
    let (rtsp, over_rtsp) = rtsp_player(&url);
    wait_for("RTSP packets", || over_rtsp.get() > 20);

    rtsp.stop();
    server.stop();
    camera.stop();
}

#[test]
fn a_camera_sending_b_frames_is_relayed_in_order() {
    let Some(camera) = Camera::with_b_frames("127.0.0.1:0".parse().unwrap()) else {
        eprintln!("skipping: no libx264 in this FFmpeg");
        return;
    };
    let server = server();
    made(&server, "cam1", camera.addr, "tcp");
    let url = play_rtsp(server.http_addr(), "cam1");
    let (_, streams) = RtspSource::open(
        "probe",
        &url,
        RtspOptions {
            transport: RtspTransport::Tcp,
            timeout: Duration::from_secs(5),
        },
    )
    .unwrap();
    assert!(streams[0].reorders(), "said to reorder, as the camera's is");
    let (rtsp, over_rtsp) = rtsp_player(&url);
    wait_for("RTSP packets", || over_rtsp.get() > 20);
    assert_eq!(state_of(&server), SourceState::Live, "and stays live");

    rtsp.stop();
    server.stop();
    camera.stop();
}

#[test]
fn a_client_to_choose_is_no_longer_asked() {
    let server = server();
    let (status, _, _) = http(
        server.http_addr(),
        "POST",
        "/api/v1/channels",
        Some(r#"{"name":"cam1","source":"rtsp://127.0.0.1:1/cam","client":"ffmpeg"}"#),
    );
    assert_eq!(status, 400, "an unknown field");
    assert!(server.status().is_empty());
    server.stop();
}
