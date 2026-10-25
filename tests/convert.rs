//! Conversions: a track an output cannot carry is converted for it — AAC to
//! Opus for WebRTC players, Opus to AAC for a recording — in time with what
//! goes on unconverted beside it, only while that output reads it, and
//! without changing what any other output is sent.

mod common;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::*;
use media_pp::elements::VideoCodec;
use media_pp::ffmpeg::codec::Id;
use media_pp::ffmpeg::media;
use relay_pp::SourceState;

/// The server's log, in the output of the test that wrote it.
fn logged() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("info")
        .with_test_writer()
        .try_init();
}

fn made(server: &relay_pp::Server, camera: std::net::SocketAddr) {
    logged();
    let body = channel("cam1", &format!("rtsp://{camera}/cam"));
    let (status, _, answer) = http(server.http_addr(), "POST", "/api/v1/channels", Some(&body));
    assert_eq!(status, 201, "{answer}");
    wait_for("the camera live", || state_of(server) == SourceState::Live);
}

/// Waits for the channel to have settled on how it pulls its camera: one
/// `retina` cannot take after all — Kvazaar's H.265 reorders its pictures
/// whatever it is asked — is connected again by FFmpeg's a moment after
/// going live.
fn settled(server: &relay_pp::Server) {
    std::thread::sleep(Duration::from_millis(2500));
    wait_for("the camera live, settled", || {
        state_of(server) == SourceState::Live
    });
}

/// A channel's `"convert"` converting pictures on the processor where no
/// GPU encoder opens.
const ANY_VIDEO: &str = r#"{"video":"any"}"#;

fn conversions(server: &relay_pp::Server) -> Vec<String> {
    server.status()[0].conversions.clone()
}

#[test]
fn aac_reaches_webrtc_players_as_opus_and_rtsp_players_as_it_is() {
    if !opus_encodes() {
        return;
    }
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
fn opus_is_recorded_as_it_came_in_time_with_the_pictures() {
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
    assert!(conversions(&server).is_empty(), "fMP4 holds Opus");

    let (codecs, starts) = recorded(&root);
    assert_eq!(codecs, [Id::H264, Id::OPUS]);
    assert_in_time(
        "opus_is_recorded_as_it_came_in_time_with_the_pictures",
        &server,
        &root,
        &starts,
    );

    server.stop();
    camera.stop();
}

#[test]
fn a_conversion_stops_once_no_webrtc_player_has_played_for_a_while() {
    if !opus_encodes() {
        return;
    }
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

/// [`first_of_each`] of the recording under `root`, once it has a packet of
/// each: a recording begins at a keyframe that comes after it was asked
/// for, the conversion's own among them, and is written a segment at a
/// time.
fn recorded(root: &Path) -> (Vec<Id>, Vec<f64>) {
    let deadline = Instant::now() + WITHIN;
    loop {
        if let Some(found) = find(root, "media.m4s").and_then(|media| first_of_each(&media)) {
            return found;
        }
        assert!(
            Instant::now() < deadline,
            "a recording with pictures and sound within {WITHIN:?}"
        );
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// The codecs of a file's pictures and sound, in that order, and where the
/// first packet of each is, in seconds — none while the file holds less.
fn first_of_each(path: &Path) -> Option<(Vec<Id>, Vec<f64>)> {
    let mut input = media_pp::ffmpeg::format::input(path).ok()?;
    let mut codecs = Vec::new();
    let mut indices = Vec::new();
    for kind in [media::Type::Video, media::Type::Audio] {
        let stream = input.streams().best(kind)?;
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
    Some((codecs, starts.into_iter().collect::<Option<Vec<_>>>()?))
}

/// That a recording's pictures and sound begin within half a second of each
/// other. Where they do not, it says what the recording holds — the first
/// packets of each, the playlist, the channel's status — and keeps the
/// recording under `CARGO_TARGET_TMPDIR/failed/{test}`, which CI uploads:
/// the failure seen only on a hosted Mac, whose cause the numbers alone
/// did not say.
fn assert_in_time(test: &str, server: &relay_pp::Server, root: &Path, starts: &[f64]) {
    let apart = (starts[0] - starts[1]).abs();
    if apart < 0.5 {
        return;
    }
    let media = find(root, "media.m4s").expect("the recording was found above");
    let packets = first_packets(&media, 24).join("\n");
    let playlist = media
        .parent()
        .and_then(|chunk| std::fs::read_dir(chunk).ok())
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "m3u8"))
        .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
        .collect::<Vec<_>>()
        .join("\n");
    let kept = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("failed")
        .join(test);
    let _ = std::fs::remove_dir_all(&kept);
    let copied = copy_dir(root, &kept);
    panic!(
        "pictures and sound {apart:.2}s apart: {starts:?}\n\
         first packets (stream, pts s, dts s, key, bytes):\n{packets}\n\
         playlist:\n{playlist}\n\
         status: {:?}\n\
         recording kept at {} ({copied:?})",
        server.status(),
        kept.display(),
    );
}

/// The first `count` packets of the file at `path`, as lines.
fn first_packets(path: &Path, count: usize) -> Vec<String> {
    let Ok(mut input) = media_pp::ffmpeg::format::input(path) else {
        return vec!["(cannot open)".into()];
    };
    input
        .packets()
        .take(count)
        .map(|(stream, packet)| {
            let unit = f64::from(stream.time_base());
            let seconds = |at: Option<i64>| at.map(|at| format!("{:.3}", at as f64 * unit));
            format!(
                "{} {:?} {:?} {} {}",
                stream.index(),
                seconds(packet.pts()),
                seconds(packet.dts()),
                packet.is_key(),
                packet.size()
            )
        })
        .collect()
}

/// Copies the directory `from` into `to`, whole.
fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

/// A camera of H.265 pictures, by whichever encoder this FFmpeg has.
fn h265_camera(sound: bool) -> Option<Camera> {
    [VideoCodec::Kvazaar, VideoCodec::H265]
        .into_iter()
        .find_map(|codec| Camera::with_pictures("127.0.0.1:0".parse().unwrap(), codec, sound))
}

#[test]
fn h265_reaches_webrtc_players_as_h264_and_rtsp_players_as_it_is() {
    let Some(camera) = h265_camera(false) else {
        eprintln!("skipping: no H.265 encoder in this FFmpeg");
        return;
    };
    let server = server();
    // On the GPU where there is one, and on the processor where not — a
    // runner of CI's — so that the conversion is tested either way.
    made_with(&server, camera.addr, ANY_VIDEO);
    settled(&server);

    let (whep, tracks) = whep_tracks(&format!("http://{}/cam1/whep", server.http_addr()));
    let codecs: Vec<Id> = tracks.iter().map(|(codec, _)| *codec).collect();
    assert_eq!(codecs, [Id::H264]);
    wait_for("pictures over WebRTC", || tracks[0].1.get() > 20);
    assert_eq!(conversions(&server), ["0:h264"]);

    let url = play_rtsp(server.http_addr(), "cam1");
    assert_eq!(rtsp_codecs(&url), [Id::HEVC], "RTSP as it came");

    whep.stop();
    server.stop();
    camera.stop();
}

#[test]
fn vp8_is_recorded_as_h264_in_time_with_the_sound() {
    let Some(camera) = Camera::with_pictures("127.0.0.1:0".parse().unwrap(), VideoCodec::Vp8, true)
    else {
        eprintln!("skipping: no VP8 encoder in this FFmpeg");
        return;
    };
    let root = scratch("convert_vp8");
    let server = server_with(&format!(
        "[record]\ndir = {:?}\nsegment = \"1s\"\nchunk = \"1h\"",
        root.display().to_string()
    ));
    made_with(&server, camera.addr, ANY_VIDEO);
    std::thread::sleep(Duration::from_secs(3));
    let (status, _, _) = http(
        server.http_addr(),
        "POST",
        "/api/v1/channels/cam1/record",
        None,
    );
    assert_eq!(status, 201);
    wait_for("a recording", || server.status()[0].recording.is_some());
    assert_eq!(conversions(&server), ["0:h264"]);

    let (codecs, starts) = recorded(&root);
    assert_eq!(codecs, [Id::H264, Id::AAC]);
    assert_in_time(
        "vp8_is_recorded_as_h264_in_time_with_the_sound",
        &server,
        &root,
        &starts,
    );

    server.stop();
    camera.stop();
}

/// Makes `cam1` pulling `camera` with `convert` — the body's `"convert"`.
fn made_with(server: &relay_pp::Server, camera: std::net::SocketAddr, convert: &str) {
    logged();
    let body = format!(r#"{{"name":"cam1","source":"rtsp://{camera}/cam","convert":{convert}}}"#);
    let (status, _, answer) = http(server.http_addr(), "POST", "/api/v1/channels", Some(&body));
    assert_eq!(status, 201, "{answer}");
    wait_for("the camera live", || state_of(server) == SourceState::Live);
}

/// A WHEP offer `POST`ed as it is, to see the answer's status and body.
fn offered(server: &relay_pp::Server) -> (u16, String) {
    use std::io::{Read, Write};
    let mut stream = std::net::TcpStream::connect(server.http_addr()).unwrap();
    let offer = "v=0\r\n";
    write!(
        stream,
        "POST /cam1/whep HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\
         Content-Type: application/sdp\r\nContent-Length: {}\r\n\r\n{offer}",
        offer.len()
    )
    .unwrap();
    let mut answer = String::new();
    stream.read_to_string(&mut answer).unwrap();
    let status = answer
        .split(' ')
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    (status, answer)
}

#[test]
fn a_channel_sound_is_not_converted_for_is_played_over_webrtc_without_it() {
    let camera = Camera::with_sound("127.0.0.1:0".parse().unwrap());
    let server = server();
    made_with(&server, camera.addr, r#"{"sound":"off"}"#);
    assert_eq!(
        server.status()[0].convert.sound,
        relay_pp::SoundConversion::Off
    );

    let (whep, tracks) = whep_tracks(&format!("http://{}/cam1/whep", server.http_addr()));
    let codecs: Vec<Id> = tracks.iter().map(|(codec, _)| *codec).collect();
    assert_eq!(codecs, [Id::H264], "its pictures alone");
    wait_for("pictures over WebRTC", || tracks[0].1.get() > 20);
    assert!(conversions(&server).is_empty());

    whep.stop();
    server.stop();
    camera.stop();
}

#[test]
fn a_channel_pictures_are_not_converted_for_refuses_webrtc_saying_why() {
    let Some(camera) = h265_camera(false) else {
        eprintln!("skipping: no H.265 encoder in this FFmpeg");
        return;
    };
    let server = server();
    made_with(&server, camera.addr, r#"{"video":"off"}"#);
    settled(&server);

    let (status, answer) = offered(&server);
    assert_eq!(
        status, 409,
        "refused for as long as the channel is: {answer}"
    );
    assert!(answer.contains("not to be converted"), "{answer}");
    assert!(conversions(&server).is_empty());
    let url = play_rtsp(server.http_addr(), "cam1");
    assert_eq!(rtsp_codecs(&url), [Id::HEVC], "RTSP goes on as it came");

    server.stop();
    camera.stop();
}

#[test]
fn a_channel_says_what_it_converts_over_the_server() {
    if !opus_encodes() {
        return;
    }
    let camera = Camera::with_sound("127.0.0.1:0".parse().unwrap());
    let server = server_with("[convert]\nsound = \"off\"");
    made_with(&server, camera.addr, r#"{"sound":"auto"}"#);

    let (whep, tracks) = whep_tracks(&format!("http://{}/cam1/whep", server.http_addr()));
    let codecs: Vec<Id> = tracks.iter().map(|(codec, _)| *codec).collect();
    assert_eq!(
        codecs,
        [Id::H264, Id::OPUS],
        "the channel's say, over the server's"
    );

    let (status, _, answer) = http(
        server.http_addr(),
        "POST",
        "/api/v1/channels",
        Some(r#"{"name":"cam2","source":"rtsp://127.0.0.1:1/cam","convert":{"video":"always"}}"#),
    );
    assert!((400..500).contains(&status), "{status} {answer}");

    whep.stop();
    server.stop();
    camera.stop();
}

#[test]
fn what_webrtc_players_read_follows_the_camera_when_it_comes_back_in_other_codecs() {
    let Some(probe) = Camera::with_opus("127.0.0.1:0".parse().unwrap()) else {
        eprintln!("skipping: no libopus in this FFmpeg");
        return;
    };
    probe.stop();
    let camera = Camera::with_sound("127.0.0.1:0".parse().unwrap());
    let camera_addr = camera.addr;
    let server = server();
    made(&server, camera_addr);
    let whep_url = format!("http://{}/cam1/whep", server.http_addr());
    let (whep, tracks) = whep_tracks(&whep_url);
    wait_for("sound over WebRTC", || tracks[1].1.get() > 20);
    assert_eq!(conversions(&server), ["1:opus"]);
    whep.stop();
    drop(whep);

    // It comes back sending Opus, which WebRTC carries: what WebRTC players
    // read is the channel's own broadcast again, nothing converted.
    camera.stop();
    wait_for("the drop noticed", || {
        state_of(&server) != SourceState::Live
    });
    let camera = Camera::with_opus(camera_addr).expect("libopus, as above");
    wait_for("the camera live again", || {
        let status = &server.status()[0];
        status.state == SourceState::Live && status.connections >= 2
    });
    wait_for("nothing converted", || conversions(&server).is_empty());
    let (whep, tracks) = whep_tracks(&whep_url);
    let codecs: Vec<Id> = tracks.iter().map(|(codec, _)| *codec).collect();
    assert_eq!(codecs, [Id::H264, Id::OPUS]);
    wait_for("sound over WebRTC, as it came", || tracks[1].1.get() > 20);
    assert!(conversions(&server).is_empty());

    whep.stop();
    server.stop();
    camera.stop();
}
