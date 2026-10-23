//! Phase 3 end to end, on the loopback alone: a camera of media-pp's own
//! played over HLS live — FFmpeg reading the playlist and its segments
//! through the server — written only while asked for, and begun again when
//! asked for after; and the page that plays a channel, with what it runs.

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
    assert!(playlist.contains("#EXTINF"), "{playlist}");
    assert!(sinks(&server).contains(&"hls"));

    let segment = playlist
        .lines()
        .find(|line| line.ends_with(".m4s"))
        .expect("a segment")
        .to_owned();
    let (status, head, _) = http_bytes(api, &format!("/cam1/{segment}"));
    assert_eq!(status, 200);
    assert!(head.contains("content-type: video/iso.segment"), "{head}");
    for path in [
        "/cam1/..%2F..%2Fsecret.m4s",
        "/cam1/nothing.m4s",
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
        http_bytes(api, &format!("/cam1/{segment}")).0,
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
