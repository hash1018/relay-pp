//! Conversions: a track an output cannot carry is converted for it — AAC to
//! Opus for WebRTC players, Opus to AAC for a recording — in time with what
//! goes on unconverted beside it, only while that output reads it, and
//! without changing what any other output is sent.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

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

fn conversions(server: &relay_pp::Server) -> Vec<String> {
    server.status()[0].conversions.clone()
}

#[test]
fn aac_reaches_webrtc_players_as_opus_and_rtsp_players_as_it_is() {
    let camera = Camera::with_sound("127.0.0.1:0".parse().unwrap());
    let server = server();
    made(&server, camera.addr);
    assert!(conversions(&server).is_empty(), "nothing converted unasked");

    let (whep, tracks) = whep_tracks(&format!("http://{}/cam1/whep", server.http_addr()));
    let codecs: Vec<Id> = tracks.iter().map(|(codec, _)| *codec).collect();
    assert_eq!(codecs, [Id::H264, Id::OPUS]);
    wait_for("pictures and sound over WebRTC", || {
        tracks.iter().all(|(_, counted)| counted.get() > 20)
    });
    assert_eq!(conversions(&server), ["1:opus"]);

    let url = play_rtsp(server.http_addr(), "cam1");
    assert_eq!(rtsp_codecs(&url), [Id::H264, Id::AAC], "RTSP as it came");

    whep.stop();
    server.stop();
    camera.stop();
}

#[test]
fn opus_is_recorded_as_aac_in_time_with_the_pictures() {
    let Some(camera) = Camera::with_opus("127.0.0.1:0".parse().unwrap()) else {
        eprintln!("skipping: no libopus in this FFmpeg");
        return;
    };
    let root = scratch("convert");
    let server = server_with(&format!(
        "[record]\ndir = {:?}\nsegment = \"1s\"\nchunk = \"1h\"",
        root.display().to_string()
    ));
    made(&server, camera.addr);
    // A few seconds into the connection: sound counted from naught would
    // be seconds before the pictures.
    std::thread::sleep(Duration::from_secs(3));
    let (status, _, _) = http(
        server.http_addr(),
        "POST",
        "/api/v1/channels/cam1/record",
        None,
    );
    assert_eq!(status, 201);
    wait_for("a recording", || server.status()[0].recording.is_some());
    assert_eq!(conversions(&server), ["1:aac"]);
    std::thread::sleep(Duration::from_secs(3));

    let media = find(&root, "media.m4s").expect("a recording's media file");
    let (codecs, starts) = first_of_each(&media);
    assert_eq!(codecs, [Id::H264, Id::AAC]);
    let apart = (starts[0] - starts[1]).abs();
    assert!(
        apart < 0.5,
        "sound {apart:.2}s from the pictures: {starts:?}"
    );

    server.stop();
    camera.stop();
}

#[test]
fn a_conversion_stops_once_no_webrtc_player_has_played_for_a_while() {
    let camera = Camera::with_sound("127.0.0.1:0".parse().unwrap());
    let server = server();
    made(&server, camera.addr);
    let whep_url = format!("http://{}/cam1/whep", server.http_addr());
    let (whep, tracks) = whep_tracks(&whep_url);
    wait_for("sound over WebRTC", || tracks[1].1.get() > 20);
    assert_eq!(conversions(&server), ["1:opus"]);
    // Dropped, the player ends its session with a `DELETE`.
    whep.stop();
    drop(whep);

    let deadline = std::time::Instant::now() + Duration::from_secs(45);
    while !conversions(&server).is_empty() {
        assert!(std::time::Instant::now() < deadline, "the conversion stops");
        std::thread::sleep(Duration::from_millis(500));
    }

    // Asked for again, it is made again.
    let (whep, tracks) = whep_tracks(&whep_url);
    wait_for("sound over WebRTC again", || tracks[1].1.get() > 20);
    assert_eq!(conversions(&server), ["1:opus"]);

    whep.stop();
    server.stop();
    camera.stop();
}

/// The first file called `name` under `dir`.
fn find(dir: &Path, name: &str) -> Option<PathBuf> {
    for entry in std::fs::read_dir(dir).ok()? {
        let path = entry.ok()?.path();
        if path.is_dir() {
            if let Some(found) = find(&path, name) {
                return Some(found);
            }
        } else if path.file_name().is_some_and(|file| file == name) {
            return Some(path);
        }
    }
    None
}

/// The codecs of a file's pictures and sound, in that order, and where the
/// first packet of each is, in seconds.
fn first_of_each(path: &Path) -> (Vec<Id>, Vec<f64>) {
    let mut input = media_pp::ffmpeg::format::input(path).expect("the recording opens");
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
    let mut starts = vec![None; 2];
    for (stream, packet) in input.packets() {
        let Some(slot) = indices.iter().position(|index| *index == stream.index()) else {
            continue;
        };
        if starts[slot].is_none()
            && let Some(pts) = packet.pts()
        {
            starts[slot] = Some(pts as f64 * f64::from(stream.time_base()));
        }
        if starts.iter().all(Option::is_some) {
            break;
        }
    }
    (
        codecs,
        starts
            .into_iter()
            .map(|start| start.expect("a packet of each"))
            .collect(),
    )
}
