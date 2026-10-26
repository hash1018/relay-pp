//! Phase 3 end to end, on the loopback alone: a camera of media-pp's own
//! played over Low-Latency HLS live — its parts handed out as they are
//! made, a player held until the part it asks for is in, and FFmpeg reading
//! the playlist and its segments through the server — written only while
//! asked for, and begun again when asked for after; and the page that plays
//! a channel, with what it runs.

mod common;

use std::time::{Duration, Instant};

use common::*;
use media_pp::ffmpeg::codec::Id;
use media_pp::ffmpeg::media;
use relay_pp::SourceState;

fn made(server: &relay_pp::Server, camera: std::net::SocketAddr) {
    let body = channel("cam1", &format!("rtsp://{camera}/cam"));
    let (status, _, answer) = http(server.http_addr(), "POST", "/api/v1/channels", Some(&body));
    assert_eq!(status, 201, "{answer}");
    wait_for("the camera live", || state_of(server) == SourceState::Live);
}

fn sinks(server: &relay_pp::Server) -> Vec<&'static str> {
    server.status()[0].sinks.clone()
}

/// What FFmpeg reads of `url`, a live HLS playlist, for `seconds`: the
/// codecs of its pictures and sound, and how many packets of each.
fn read_live(url: &str, seconds: u64) -> (Vec<Id>, Vec<usize>) {
    let mut input = media_pp::ffmpeg::format::input(url).expect("the playlist opens");
    let mut codecs = Vec::new();
    let mut indices = Vec::new();
    for kind in [media::Type::Video, media::Type::Audio] {
        let stream = input
            .streams()
            .best(kind)
            .expect("a picture and a sound track");
        codecs.push(stream.parameters().id());
        indices.push(stream.index());
    }
    let mut counts = vec![0; 2];
    let until = Instant::now() + Duration::from_secs(seconds);
    let mut packet = media_pp::ffmpeg::Packet::empty();
    while Instant::now() < until && packet.read(&mut input).is_ok() {
        if let Some(slot) = indices.iter().position(|index| *index == packet.stream()) {
            counts[slot] += 1;
        }
    }
    (codecs, counts)
}

#[test]
fn a_channel_is_played_over_hls_while_it_is_asked_for() {
    let camera = Camera::with_sound("127.0.0.1:0".parse().unwrap());
    let server = server_with("[hls]\nidle = \"2s\"");
    made(&server, camera.addr);
    let api = server.http_addr();
    assert!(!sinks(&server).contains(&"hls"), "nothing written unasked");

    let (status, head, playlist) = http(api, "GET", "/cam1/index.m3u8", None);
    assert_eq!(status, 200, "{playlist}");
    assert!(
        head.contains("content-type: application/vnd.apple.mpegurl"),
        "{head}"
    );
    assert!(
        playlist.contains("#EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=YES"),
        "{playlist}"
    );
    assert!(playlist.contains("#EXT-X-PART:"), "{playlist}");
    assert!(sinks(&server).contains(&"hls"));

    // A part, and the part a player is told is coming, which is handed out
    // once it is in.
    let uri = |line: &str| {
        line.split("URI=\"")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .map(str::to_owned)
    };
    let part = playlist
        .lines()
        .filter(|line| line.starts_with("#EXT-X-PART:"))
        .find_map(uri)
        .expect("a part");
    let (status, head, _) = http_bytes(api, &format!("/cam1/{part}"));
    assert_eq!(status, 200, "{part}");
    assert!(head.contains("content-type: video/mp4"), "{head}");
    // A segment's first part is told coming only once it has begun: asked
    // between two segments, the playlist tells of none for a moment.
    let mut playlist = String::new();
    let mut coming = None;
    let deadline = Instant::now() + Duration::from_secs(3);
    while coming.is_none() && Instant::now() < deadline {
        playlist = http(api, "GET", "/cam1/index.m3u8", None).2;
        coming = playlist
            .lines()
            .filter(|line| line.starts_with("#EXT-X-PRELOAD-HINT:"))
            .find_map(uri);
    }
    let coming = coming.expect("a part coming");
    let (status, _, bytes) = http_bytes(api, &format!("/cam1/{coming}"));
    assert_eq!(status, 200, "{coming}");
    assert!(!bytes.is_empty());

    // A player saying where it is is held until the segment after is in.
    let next: u64 = playlist
        .lines()
        .find_map(|line| line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:"))
        .and_then(|first| first.parse::<u64>().ok())
        .expect("a media sequence")
        + playlist
            .lines()
            .filter(|line| line.starts_with("#EXTINF"))
            .count() as u64
        + 1;
    let asked = Instant::now();
    let (status, _, held) = http(
        api,
        "GET",
        &format!("/cam1/index.m3u8?_HLS_msn={next}&_HLS_part=0"),
        None,
    );
    assert_eq!(status, 200, "{held}");
    assert!(held.contains(&format!("part-{next}-0.mp4")), "{held}");
    assert!(asked.elapsed() < Duration::from_secs(6), "answered once in");
    let (status, _, _) = http(api, "GET", "/cam1/index.m3u8?_HLS_msn=100000", None);
    assert_eq!(status, 400, "too far ahead");

    for path in [
        "/cam1/..%2F..%2Fsecret.mp4",
        "/cam1/nothing.mp4",
        "/cam1/part-99999-0.mp4",
        "/cam1/a.txt",
    ] {
        assert_eq!(http_bytes(api, path).0, 404, "{path}");
    }

    // FFmpeg plays it live through the server, reloading the playlist as
    // it slides.
    let (codecs, counts) = read_live(&format!("http://{api}/cam1/index.m3u8"), 4);
    assert_eq!(codecs, [Id::H264, Id::AAC]);
    assert!(
        counts[0] >= 30 && counts[1] >= 30,
        "pictures and sound for seconds: {counts:?}"
    );

    // Unasked for, it ends, its files with it.
    wait_for("HLS let go of", || !sinks(&server).contains(&"hls"));
    assert_eq!(
        http_bytes(api, &format!("/cam1/{part}")).0,
        404,
        "nothing served once it ended"
    );

    // Asked for again, it is written again.
    let (status, _, playlist) = http(api, "GET", "/cam1/index.m3u8", None);
    assert_eq!(status, 200, "{playlist}");
    assert!(sinks(&server).contains(&"hls"));

    assert_eq!(http(api, "GET", "/cam9/index.m3u8", None).0, 404);
    server.stop();
    camera.stop();
}

#[test]
fn the_page_that_plays_a_channel_is_served_with_what_it_runs() {
    let server = server();
    let api = server.http_addr();

    let (status, head, page) = http(api, "GET", "/cam1", None);
    assert_eq!(status, 200);
    assert!(head.contains("content-type: text/html"), "{head}");
    assert!(
        page.contains("/whep") && page.contains("/index.m3u8"),
        "{page}"
    );
    assert!(page.contains("/assets/hls.light.min.js"));

    let (status, head, script) = http(api, "GET", "/assets/hls.light.min.js", None);
    assert_eq!(status, 200);
    assert!(head.contains("content-type: text/javascript"), "{head}");
    assert_eq!(script.len(), 385_716, "hls.js as released");
    server.stop();
}

/// With `player_page = false` the page and what it runs are not served,
/// where a server's players are of their own; the API is as ever.
#[test]
fn the_page_is_not_served_where_it_is_turned_off() {
    let config = relay_pp::Config::parse(
        r#"
        [rtsp]
        listen = "127.0.0.1:0"
        [webrtc]
        listen = "127.0.0.1:0"
        [http]
        listen = "127.0.0.1:0"
        player_page = false
        [rtmp]
        listen = "127.0.0.1:0"
        "#,
    )
    .unwrap();
    let server = relay_pp::Server::start(config).expect("start");
    let api = server.http_addr();
    assert_eq!(http(api, "GET", "/cam1", None).0, 404, "no page");
    assert_eq!(
        http(api, "GET", "/assets/hls.light.min.js", None).0,
        404,
        "nor what it runs"
    );
    let body = channel("cam1", "rtsp://127.0.0.1:1/cam");
    assert_eq!(
        http(api, "POST", "/api/v1/channels", Some(&body)).0,
        201,
        "the API as ever"
    );
    assert_eq!(http(api, "GET", "/api/v1/channels/cam1", None).0, 200);
    server.stop();
}
