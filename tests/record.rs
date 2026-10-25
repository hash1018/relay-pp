//! Phase 2 end to end, on the loopback alone: a camera of media-pp's own
//! recorded as HLS, chunk after chunk, a stretch of it played back as one
//! playlist, read back by FFmpeg through the server, and a recording ended
//! and begun again as the camera drops and comes back.

mod common;

use std::path::Path;
use std::time::Duration;

use common::*;
use relay_pp::SourceState;

/// The chunks of `channel` under `root`, oldest first: their directories.
fn chunks(root: &Path, channel: &str) -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    for date in std::fs::read_dir(root.join(channel))
        .into_iter()
        .flatten()
        .flatten()
    {
        for chunk in std::fs::read_dir(date.path())
            .into_iter()
            .flatten()
            .flatten()
        {
            found.push(chunk.path());
        }
    }
    found.sort();
    found
}

/// Each chunk of `channel` under `root`, as a line: its name, whether its
/// playlist is ended, its segments and their length, and the pictures its
/// media file holds — how many, and from when to when. Where a playback is
/// short, whether a chunk was short or the playback lost some of one.
fn described(root: &Path, channel: &str) -> String {
    chunks(root, channel)
        .iter()
        .map(|chunk| {
            let listed = playlist(chunk);
            let segments: Vec<f64> = listed
                .lines()
                .filter_map(|line| line.strip_prefix("#EXTINF:"))
                .filter_map(|rest| rest.trim_end_matches(',').parse().ok())
                .collect();
            let pictures = media_pp::ffmpeg::format::input(&chunk.join("media.m4s"))
                .ok()
                .and_then(|mut input| {
                    let stream = input.streams().best(media_pp::ffmpeg::media::Type::Video)?;
                    let (index, unit) = (stream.index(), f64::from(stream.time_base()));
                    let times: Vec<f64> = input
                        .packets()
                        .filter(|(stream, _)| stream.index() == index)
                        .filter_map(|(_, packet)| packet.pts())
                        .map(|pts| pts as f64 * unit)
                        .collect();
                    let first = times.iter().copied().reduce(f64::min)?;
                    let last = times.iter().copied().reduce(f64::max)?;
                    Some(format!("{} pictures, {first:.3}..{last:.3} s", times.len()))
                })
                .unwrap_or_else(|| "no pictures read".into());
            format!(
                "{} ended={} segments={} ({:.2} s) {pictures}",
                chunk.file_name().unwrap_or_default().to_string_lossy(),
                listed.contains("#EXT-X-ENDLIST"),
                segments.len(),
                segments.iter().sum::<f64>(),
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn playlist(chunk: &Path) -> String {
    std::fs::read_to_string(chunk.join("index.m3u8")).unwrap_or_default()
}

#[test]
fn a_camera_is_recorded_chunk_by_chunk_and_played_back() {
    let camera = Camera::start("127.0.0.1:0".parse().unwrap());
    let camera_addr = camera.addr;
    let root = scratch("record");
    let server = server_with(&format!(
        "[record]\ndir = {:?}\nsegment = \"1s\"\nchunk = \"4s\"",
        root.display().to_string()
    ));
    let api = server.http_addr();
    let body = channel("floor2-cam1", &format!("rtsp://{camera_addr}/cam"));
    let (status, _, answer) = http(api, "POST", "/api/v1/channels", Some(&body));
    assert_eq!(status, 201, "{answer}");
    assert!(
        answer.contains(r#""sinks":[]"#),
        "made with its source alone: {answer}"
    );
    let record = |method| http(api, method, "/api/v1/channels/floor2-cam1/record", None).0;
    assert_eq!(record("POST"), 201);
    assert_eq!(record("POST"), 409, "recorded already");
    assert_eq!(server.status()[0].sinks, ["record"]);

    // A chunk ends at each whole four seconds, its playlist ended, and the
    // next begins.
    wait_for("a recording", || server.status()[0].recording.is_some());
    wait_for("a chunk finished and another begun", || {
        let found = chunks(&root, "floor2-cam1");
        found.len() >= 2 && playlist(&found[0]).contains("#EXT-X-ENDLIST")
    });
    let first = chunks(&root, "floor2-cam1").remove(0);
    let files: Vec<_> = std::fs::read_dir(&first)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(files.len(), 2, "a playlist and one media file: {files:?}");
    let recorded = playlist(&first);
    assert!(recorded.contains("#EXT-X-BYTERANGE:"), "{recorded}");

    // Listed, and played back as one playlist across the chunks.
    let (status, _, listed) = http(api, "GET", "/api/v1/recordings/floor2-cam1", None);
    assert_eq!(status, 200, "{listed}");
    assert!(listed.matches(r#""start""#).count() >= 2, "{listed}");
    wait_for("two finished chunks", || {
        chunks(&root, "floor2-cam1")
            .iter()
            .filter(|chunk| playlist(chunk).contains("#EXT-X-ENDLIST"))
            .count()
            >= 2
    });
    let (status, _, played) = http(api, "GET", "/playback/floor2-cam1/index.m3u8", None);
    assert_eq!(status, 200, "{played}");
    assert!(played.contains("#EXT-X-PLAYLIST-TYPE:VOD") && played.contains("#EXT-X-ENDLIST"));
    assert!(played.contains("#EXT-X-DISCONTINUITY"), "{played}");
    assert!(played.contains("#EXT-X-PROGRAM-DATE-TIME:"), "{played}");
    assert!(played.contains("/recordings/floor2-cam1/"), "{played}");
    let (status, _, _) = http(
        api,
        "GET",
        "/playback/floor2-cam1/index.m3u8?from=soon",
        None,
    );
    assert_eq!(status, 400);
    let (status, _, _) = http(
        api,
        "GET",
        "/playback/floor2-cam1/index.m3u8?to=2000-01-01T00:00:00Z",
        None,
    );
    assert_eq!(status, 404, "nothing recorded then");

    // A segment is a range of the one file, served as one.
    let media = played
        .lines()
        .find(|line| line.starts_with("/recordings/"))
        .unwrap()
        .to_owned();
    let (status, length) = http_range(api, &media, "0-99");
    assert_eq!((status, length), (206, 100));

    // FFmpeg plays the stretch back through the server, ranges and all:
    // four seconds or more of pictures. Measured by their timestamps, not
    // counted: a hosted runner's camera, encoding live, may drop pictures,
    // and a count of 60 failed there at 56 without saying which. Each chunk
    // begins its own timeline, so the stretch is the runs between the
    // places the timestamps go back, added up.
    let url = format!("http://{api}/playback/floor2-cam1/index.m3u8");
    let mut input = media_pp::ffmpeg::format::input(&url).expect("the playback opens");
    let pictures = input
        .streams()
        .best(media_pp::ffmpeg::media::Type::Video)
        .expect("the playback has pictures");
    let (index, unit) = (pictures.index(), f64::from(pictures.time_base()));
    let (mut stretch, mut run, mut last, mut packets) = (0.0, None::<(f64, f64)>, None, 0);
    let mut packet = media_pp::ffmpeg::Packet::empty();
    while packet.read(&mut input).is_ok() {
        packets += 1;
        let Some(at) = packet.pts().filter(|_| packet.stream() == index) else {
            continue;
        };
        let at = at as f64 * unit;
        if last.is_some_and(|last| at < last) {
            let (from, to) = run.take().expect("a run under way");
            stretch += to - from + 1.0 / 15.0;
        }
        run = Some(run.map_or((at, at), |(from, to): (f64, f64)| (from, to.max(at))));
        last = Some(at);
    }
    drop(input);
    if let Some((from, to)) = run {
        stretch += to - from + 1.0 / 15.0;
    }
    assert!(
        stretch >= 3.9,
        "four seconds or more of pictures: {stretch:.2} s in {packets} packets\n\
         the playback:\n{played_list}\n\
         the chunks:\n{}",
        described(&root, "floor2-cam1"),
        played_list = played,
    );

    // The camera drops: the recording under way ends with it. It comes
    // back: another begins. A new chunk, not more of them: the one under
    // way as the camera dropped may have had no keyframe yet, and is then
    // removed as it ends.
    let before = chunks(&root, "floor2-cam1");
    camera.stop();
    wait_for("the drop noticed", || {
        server.status()[0].state != SourceState::Live
    });
    wait_for("the recording ended", || {
        server.status()[0].recording.is_none()
    });
    let last = chunks(&root, "floor2-cam1").pop().unwrap();
    assert!(
        playlist(&last).contains("#EXT-X-ENDLIST"),
        "ended with the camera"
    );
    let camera = Camera::start(camera_addr);
    wait_for("recording again", || server.status()[0].recording.is_some());
    let after = chunks(&root, "floor2-cam1");
    assert!(
        after.iter().any(|chunk| !before.contains(chunk)),
        "a chunk begun as the camera came back: {before:?} then {after:?}"
    );

    // Taken off while the camera streams: answered once the recording is
    // finished. Put back on: another begins at once.
    assert_eq!(record("DELETE"), 204);
    assert!(server.status()[0].recording.is_none());
    assert!(server.status()[0].sinks.is_empty());
    let last = chunks(&root, "floor2-cam1").pop().unwrap();
    assert!(
        playlist(&last).contains("#EXT-X-ENDLIST"),
        "finished as it was taken off"
    );
    assert_eq!(record("DELETE"), 404, "not recorded");
    let (status, _, _) = http(api, "DELETE", "/api/v1/channels/nowhere/record", None);
    assert_eq!(status, 404);
    let before = chunks(&root, "floor2-cam1").len();
    assert_eq!(record("POST"), 201);
    wait_for("recording once more", || {
        server.status()[0].recording.is_some()
    });
    assert_eq!(chunks(&root, "floor2-cam1").len(), before + 1);

    // Stopping the server ends the recording under way too.
    server.stop();
    let last = chunks(&root, "floor2-cam1").pop().unwrap();
    assert!(
        playlist(&last).contains("#EXT-X-ENDLIST"),
        "ended with the server"
    );
    camera.stop();
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn a_channel_not_recorded_writes_nothing() {
    let camera = Camera::start("127.0.0.1:0".parse().unwrap());
    let root = scratch("not_recorded");
    let server = server_with(&format!("[record]\ndir = {:?}", root.display().to_string()));
    server.add_channel(&pulling("cam1", camera.addr)).unwrap();
    wait_for("live", || state_of(&server) == SourceState::Live);
    std::thread::sleep(Duration::from_secs(1));
    assert!(server.status()[0].recording.is_none());
    assert!(chunks(&root, "cam1").is_empty());
    server.stop();
    camera.stop();
    std::fs::remove_dir_all(&root).unwrap();
}
