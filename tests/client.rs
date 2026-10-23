//! Both RTSP clients a camera is pulled with — `retina`'s and FFmpeg's —
//! behave alike from outside: the same camera is pulled, played over RTSP
//! and WHEP, and followed across a drop by either, and a camera `retina`
//! cannot take is pulled by FFmpeg's without being asked.

mod common;

use std::net::SocketAddr;
use std::time::Duration;

use common::*;
use media_pp::elements::{RtspOptions, RtspSource, RtspTransport};
use relay_pp::SourceState;

/// Makes `name` pulling the camera at `camera` with `client` — or the
/// default, where `None` — and waits for it to go live.
fn made(server: &relay_pp::Server, name: &str, camera: SocketAddr, client: Option<&str>) {
    let body = match client {
        Some(client) => {
            format!(r#"{{"name":"{name}","source":"rtsp://{camera}/cam","client":"{client}"}}"#)
        }
        None => channel(name, &format!("rtsp://{camera}/cam")),
    };
    let (status, _, answer) = http(server.http_addr(), "POST", "/api/v1/channels", Some(&body));
    assert_eq!(status, 201, "{answer}");
    wait_for("the camera live", || state_of(server) == SourceState::Live);
}

/// The channel played over RTSP and WHEP, and followed across the camera
/// dropping and coming back, by the same client throughout.
fn played_and_followed(client: Option<&str>, expected: &str) {
    let camera = Camera::start("127.0.0.1:0".parse().unwrap());
    let camera_addr = camera.addr;
    let server = server();
    made(&server, "cam1", camera_addr, client);
    assert_eq!(server.status()[0].client, Some(expected));
    let (_, _, listed) = http(server.http_addr(), "GET", "/api/v1/channels/cam1", None);
    assert!(
        listed.contains(&format!(r#""client":"{expected}""#)),
        "{listed}"
    );

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
    assert_eq!(server.status()[0].client, Some(expected));
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
fn retina_pulls_a_camera_it_takes_by_default() {
    played_and_followed(None, "retina");
}

#[test]
fn ffmpeg_pulls_a_camera_when_asked() {
    played_and_followed(Some("ffmpeg"), "ffmpeg");
}

#[test]
fn a_camera_retina_cannot_take_is_pulled_by_ffmpeg_with_every_track() {
    // Two tracks and no `rtptime` to line them up by: `retina` says no.
    let camera = Camera::with_sound("127.0.0.1:0".parse().unwrap());
    let server = server();
    made(&server, "cam1", camera.addr, None);
    assert_eq!(server.status()[0].client, Some("ffmpeg"));

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
fn an_unknown_client_is_refused() {
    let server = server();
    let (status, _, _) = http(
        server.http_addr(),
        "POST",
        "/api/v1/channels",
        Some(r#"{"name":"cam1","source":"rtsp://127.0.0.1:1/cam","client":"gstreamer"}"#),
    );
    assert!((400..500).contains(&status), "{status}");
    assert!(server.status().is_empty());
    server.stop();
}
