//! Who may do what, against real clients: the API by the manager's token,
//! RTSP players — FFmpeg's, answering a Digest challenge — as users or with
//! a play token, WebRTC players with a token, HLS players and a recording
//! watched with one carried in their URLs.

mod common;

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use common::*;
use hmac::{Hmac, Mac};
use media_pp::elements::{WhepOptions, WhepSource};
use relay_pp::SourceState;
use sha2::Sha256;

const API_TOKEN: &str = "manager-token-0123456789";
const SECRET: &str = "a-secret-of-thirty-two-characters!";

fn auth(more: &str) -> String {
    format!(
        "[auth]\napi_token = {API_TOKEN:?}\ntoken_secret = {SECRET:?}\n\
         [[auth.users]]\nname = \"guard\"\npassword = \"pw\"\nplay = [\"cam*\"]\n{more}"
    )
}

fn manager() -> Option<&'static str> {
    Some(Box::leak(format!("Bearer {API_TOKEN}").into_boxed_str()))
}

/// A play token of `claims` besides an expiry ten minutes on, as a manager
/// would sign it.
fn token(claims: &str) -> String {
    let exp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 600;
    let header = URL_SAFE_NO_PAD.encode(r#"{"alg":"HS256","typ":"JWT"}"#);
    let payload = URL_SAFE_NO_PAD.encode(format!(r#"{{"exp":{exp},{claims}}}"#));
    let signed = format!("{header}.{payload}");
    let mut mac = Hmac::<Sha256>::new_from_slice(SECRET.as_bytes()).unwrap();
    mac.update(signed.as_bytes());
    format!(
        "{signed}.{}",
        URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
    )
}

fn made(server: &relay_pp::Server, camera: std::net::SocketAddr) {
    let body = channel("cam1", &format!("rtsp://{camera}/cam"));
    let (status, _, answer) = http_as(
        server.http_addr(),
        "POST",
        "/api/v1/channels",
        Some(&body),
        manager(),
    );
    assert_eq!(status, 201, "{answer}");
    wait_for("the camera live", || state_of(server) == SourceState::Live);
}

#[test]
fn the_api_takes_the_managers_token_alone() {
    let server = server_with(&auth(""));
    let api = server.http_addr();
    let body = channel("cam1", "rtsp://127.0.0.1:1/cam");

    let (status, head, _) = http(api, "POST", "/api/v1/channels", Some(&body));
    assert_eq!(status, 401);
    assert!(head.contains("www-authenticate: Bearer"), "{head}");
    let (status, _, _) = http_as(
        api,
        "POST",
        "/api/v1/channels",
        Some(&body),
        Some("Bearer not-the-token"),
    );
    assert_eq!(status, 401);
    // A player's token is no manager's, whatever it lets it play.
    let player = format!("Bearer {}", token(r#""play":["*"]"#));
    let (status, _, _) = http_as(api, "GET", "/api/v1/channels", None, Some(&player));
    assert_eq!(status, 401);

    let (status, _, answer) = http_as(api, "POST", "/api/v1/channels", Some(&body), manager());
    assert_eq!(status, 201, "{answer}");
    let (status, _, listed) = http_as(api, "GET", "/api/v1/channels", None, manager());
    assert_eq!(status, 200);
    assert!(listed.contains(r#""name":"cam1""#), "{listed}");

    // The metrics are the manager's too: they name every channel.
    assert_eq!(http(api, "GET", "/metrics", None).0, 401);
    assert_eq!(http_as(api, "GET", "/metrics", None, Some(&player)).0, 401);
    let (status, _, metrics) = http_as(api, "GET", "/metrics", None, manager());
    assert_eq!(status, 200);
    assert!(
        metrics.contains(r#"relay_pp_channel_up{channel="cam1"}"#),
        "{metrics}"
    );
    server.stop();
}

#[test]
fn rtsp_players_play_as_users_or_with_a_token() {
    let camera = Camera::start("127.0.0.1:0".parse().unwrap());
    let server = server_with(&auth(""));
    made(&server, camera.addr);
    let play = |authorization: Option<&str>| {
        http_as(
            server.http_addr(),
            "POST",
            "/api/v1/channels/cam1/play",
            Some(r#"{"protocol":"rtsp"}"#),
            authorization,
        )
    };
    assert_eq!(play(None).0, 401, "no URL without who asks");
    let (status, _, answer) = play(Some("Basic Z3VhcmQ6cHc=")); // guard:pw
    assert_eq!(status, 200, "{answer}");
    let url: serde_json::Value = serde_json::from_str(&answer).unwrap();
    let url = url["url"].as_str().unwrap().to_owned();
    let with = |user: &str, password: &str| {
        url.replacen("rtsp://", &format!("rtsp://{user}:{password}@"), 1)
    };

    assert!(!rtsp_opens(&url), "nobody");
    assert!(rtsp_opens(&with("guard", "pw")), "a user, by Digest");
    assert!(!rtsp_opens(&with("guard", "wrong")), "a wrong password");
    let with_token = |claims: &str| format!("{url}?token={}", token(claims));
    assert!(
        rtsp_opens(&with_token(r#""play":["cam1"]"#)),
        "a token in the URL's query"
    );
    assert!(
        !rtsp_opens(&with_token(r#""play":["door*"]"#)),
        "a token for other channels"
    );
    server.stop();
    camera.stop();
}

#[test]
fn webrtc_players_present_a_token() {
    let camera = Camera::start("127.0.0.1:0".parse().unwrap());
    let server = server_with(&auth(""));
    made(&server, camera.addr);
    let url = format!("http://{}/cam1/whep", server.http_addr());
    let open = |bearer_token: Option<String>| {
        WhepSource::open(
            "player",
            &url,
            WhepOptions {
                bearer_token,
                ..WhepOptions::default()
            },
        )
        .map(|_| ())
    };
    assert!(open(None).is_err(), "nobody");
    assert!(
        open(Some(token(r#""play":["door*"]"#))).is_err(),
        "other channels"
    );
    assert!(open(Some(token(r#""play":["cam1"]"#))).is_ok(), "{url}");
    server.stop();
    camera.stop();
}

/// An HLS player sets no header: its token comes in the playlist's URL,
/// and goes on in each segment's the playlist names. The page that plays a
/// channel is anybody's; what it plays is not.
#[test]
fn hls_players_carry_a_token_in_their_urls() {
    let camera = Camera::start("127.0.0.1:0".parse().unwrap());
    let server = server_with(&auth(""));
    made(&server, camera.addr);
    let api = server.http_addr();

    assert_eq!(http(api, "GET", "/cam1/index.m3u8", None).0, 401);
    let elsewhere = token(r#""play":["door*"]"#);
    let path = format!("/cam1/index.m3u8?token={elsewhere}");
    assert_eq!(http(api, "GET", &path, None).0, 401, "a token for others");

    let play = token(r#""play":["cam1"]"#);
    let (status, _, playlist) = http(api, "GET", &format!("/cam1/index.m3u8?token={play}"), None);
    assert_eq!(status, 200, "{playlist}");
    let segment = playlist
        .lines()
        .find(|line| line.contains(".m4s"))
        .expect("a segment")
        .to_owned();
    assert!(segment.ends_with(&format!("?token={play}")), "{segment}");
    let bare = segment.split('?').next().unwrap();
    assert_eq!(http_bytes(api, &format!("/cam1/{bare}")).0, 401);
    assert_eq!(http_bytes(api, &format!("/cam1/{segment}")).0, 200);

    // FFmpeg follows it, segment by segment.
    let url = format!("http://{api}/cam1/index.m3u8?token={play}");
    let mut input = media_pp::ffmpeg::format::input(&url).expect("the playlist opens");
    let mut packet = media_pp::ffmpeg::Packet::empty();
    let mut packets = 0;
    while packets < 30 && packet.read(&mut input).is_ok() {
        packets += 1;
    }
    assert_eq!(packets, 30);
    drop(input);

    assert_eq!(http(api, "GET", "/cam1", None).0, 200, "the page");
    server.stop();
    camera.stop();
}

#[test]
fn a_recording_is_watched_with_a_token_carried_on_in_its_urls() {
    let camera = Camera::start("127.0.0.1:0".parse().unwrap());
    let root = scratch("auth_record");
    let server = server_with(&auth(&format!(
        "[record]\ndir = {:?}\nsegment = \"1s\"\nchunk = \"1h\"",
        root.display().to_string()
    )));
    made(&server, camera.addr);
    let api = server.http_addr();
    let (status, _, _) = http_as(api, "POST", "/api/v1/channels/cam1/record", None, manager());
    assert_eq!(status, 201);
    wait_for("a recording", || server.status()[0].recording.is_some());

    let watcher = token(r#""recordings":["cam1"]"#);
    let player = token(r#""play":["cam1"]"#);
    let playback = |token: &str| {
        http(
            api,
            "GET",
            &format!("/playback/cam1/index.m3u8?token={token}"),
            None,
        )
    };
    assert_eq!(http(api, "GET", "/playback/cam1/index.m3u8", None).0, 401);
    assert_eq!(playback(&player).0, 401, "playing live is not watching");
    let deadline = std::time::Instant::now() + WITHIN;
    let played = loop {
        let (status, _, played) = playback(&watcher);
        if status == 200 && played.contains("/recordings/") {
            break played;
        }
        assert!(std::time::Instant::now() < deadline, "{status} {played}");
        std::thread::sleep(Duration::from_millis(500));
    };
    let media = played
        .lines()
        .find(|line| line.starts_with("/recordings/"))
        .unwrap();
    assert!(media.ends_with(&format!("?token={watcher}")), "{media}");
    assert_eq!(http_range(api, media, "0-99"), (206, 100), "with its token");
    let bare = media.split('?').next().unwrap();
    assert_eq!(http_range(api, bare, "0-99").0, 401, "without");
    let (status, _, _) = http_as(
        api,
        "GET",
        "/api/v1/recordings/cam1",
        None,
        Some(&format!("Bearer {watcher}")),
    );
    assert_eq!(status, 200, "listed with it too");
    server.stop();
    camera.stop();
}

#[test]
fn publishers_present_a_token_that_lets_them_publish() {
    let server = server_with(&auth(""));
    for (name, publish) in [("studio", "rtmp"), ("browser", "whip")] {
        let body = format!(r#"{{"name":"{name}","publish":"{publish}"}}"#);
        let (status, _, answer) = http_as(
            server.http_addr(),
            "POST",
            "/api/v1/channels",
            Some(&body),
            manager(),
        );
        assert_eq!(status, 201, "{answer}");
    }
    let rtmp = format!("rtmp://{}/live/studio", server.rtmp_addr());
    assert!(
        push(&rtmp, Push::Rtmp, None).unwrap().is_err(),
        "nobody publishes"
    );
    let player = token(r#""play":["*"]"#);
    assert!(
        push(&format!("{rtmp}?token={player}"), Push::Rtmp, None)
            .unwrap()
            .is_err(),
        "playing is not publishing"
    );
    let publisher = token(r#""publish":["studio","browser"]"#);
    let pushed = push(&format!("{rtmp}?token={publisher}"), Push::Rtmp, None)
        .unwrap()
        .expect("a token that lets it publish");
    wait_for("the publisher live", || {
        server
            .status()
            .iter()
            .any(|channel| channel.name == "studio" && channel.state == SourceState::Live)
    });
    pushed.stop();

    let whip = format!("http://{}/browser/whip", server.http_addr());
    if let Some(refused) = push(&whip, Push::Whip, None) {
        assert!(refused.is_err(), "nobody publishes over WHIP");
        let pushed = push(&whip, Push::Whip, Some(publisher))
            .unwrap()
            .expect("with its token");
        pushed.stop();
    }
    server.stop();
}
