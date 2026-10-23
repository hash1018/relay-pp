//! Published sources: a channel made to take a publisher over one protocol
//! — RTMP as OBS pushes, RTSP as an encoder, WHIP as a browser — played by
//! every player, converted where a player cannot carry what was pushed, and
//! followed as its publisher leaves and comes back.

mod common;

use std::time::Duration;

use common::*;
use media_pp::ffmpeg::codec::Id;
use relay_pp::{Server, SourceState};

fn made(server: &Server, name: &str, publish: &str) {
    let body = format!(r#"{{"name":"{name}","publish":"{publish}"}}"#);
    let (status, _, answer) = http(server.http_addr(), "POST", "/api/v1/channels", Some(&body));
    assert_eq!(status, 201, "{answer}");
}

fn conversions(server: &Server) -> Vec<String> {
    server.status()[0].conversions.clone()
}

fn rtmp_url(server: &Server, name: &str) -> String {
    format!("rtmp://{}/live/{name}", server.rtmp_addr())
}

#[test]
fn obs_over_rtmp_reaches_every_player_and_is_followed_back() {
    let server = server();
    made(&server, "studio", "rtmp");
    assert_eq!(state_of(&server), SourceState::AwaitingPublisher);
    assert_eq!(server.status()[0].client, Some("rtmp"));

    let publisher = push(&rtmp_url(&server, "studio"), Push::Rtmp, None)
        .expect("AAC is FFmpeg's own")
        .expect("the server takes it");
    wait_for("the publisher live", || {
        state_of(&server) == SourceState::Live
    });

    // AAC pushed: WebRTC players are given Opus, RTSP players AAC.
    let (whep, tracks) = whep_tracks(&format!("http://{}/studio/whep", server.http_addr()));
    let codecs: Vec<Id> = tracks.iter().map(|(codec, _)| *codec).collect();
    assert_eq!(codecs, [Id::H264, Id::OPUS]);
    wait_for("pictures and sound over WebRTC", || {
        tracks.iter().all(|(_, counted)| counted.get() > 20)
    });
    assert_eq!(conversions(&server), ["1:opus"]);
    let url = play_rtsp(server.http_addr(), "studio");
    assert_eq!(rtsp_codecs(&url), [Id::H264, Id::AAC]);

    // It leaves, and comes back: a run of its own, the players' channel
    // the same.
    publisher.stop();
    drop(publisher);
    wait_for("the publisher gone", || {
        state_of(&server) == SourceState::AwaitingPublisher
    });
    let publisher = push(&rtmp_url(&server, "studio"), Push::Rtmp, None)
        .unwrap()
        .expect("taken again");
    wait_for("back", || {
        let status = &server.status()[0];
        status.state == SourceState::Live && status.connections == 2
    });
    let before = tracks[0].1.get();
    wait_for("WebRTC pictures again", || tracks[0].1.get() > before + 20);

    whep.stop();
    publisher.stop();
    server.stop();
}

#[test]
fn an_encoder_over_rtsp_publishes_where_players_play() {
    let server = server();
    made(&server, "studio", "rtsp");
    let url = format!("rtsp://{}/studio", server.rtsp_addr());
    let publisher = push(&url, Push::Rtsp, None)
        .unwrap()
        .expect("the server takes it");
    wait_for("the publisher live", || {
        state_of(&server) == SourceState::Live
    });

    // Players play at the URL it pushes to.
    assert_eq!(play_rtsp(server.http_addr(), "studio"), url);
    let (player, counted) = rtsp_player(&url);
    wait_for("RTSP packets", || counted.get() > 20);
    let (whep, tracks) = whep_tracks(&format!("http://{}/studio/whep", server.http_addr()));
    let codecs: Vec<Id> = tracks.iter().map(|(codec, _)| *codec).collect();
    assert_eq!(codecs, [Id::H264, Id::OPUS]);

    whep.stop();
    player.stop();
    publisher.stop();
    server.stop();
}

#[test]
fn a_browser_over_whip_is_played_and_a_second_is_refused() {
    let server = server();
    made(&server, "studio", "whip");
    let url = format!("http://{}/studio/whip", server.http_addr());
    let Some(made) = push(&url, Push::Whip, None) else {
        eprintln!("skipping: no libopus in this FFmpeg");
        return;
    };
    let publisher = made.expect("the server takes it");
    wait_for("the publisher live", || {
        state_of(&server) == SourceState::Live
    });

    // Opus pushed: RTSP carries it as it came.
    let rtsp = play_rtsp(server.http_addr(), "studio");
    assert_eq!(rtsp_codecs(&rtsp), [Id::H264, Id::OPUS]);
    let (player, counted) = rtsp_player(&rtsp);
    wait_for("RTSP packets", || counted.get() > 20);
    assert!(conversions(&server).is_empty());

    assert!(
        push(&url, Push::Whip, None).unwrap().is_err(),
        "one publisher at a time"
    );

    player.stop();
    publisher.stop();
    server.stop();
}

#[test]
fn a_channel_takes_its_publisher_over_its_protocol_alone() {
    let server = server();
    made(&server, "studio", "rtmp");
    let rtsp = format!("rtsp://{}/studio", server.rtsp_addr());
    assert!(push(&rtsp, Push::Rtsp, None).unwrap().is_err(), "RTSP");
    let whip = format!("http://{}/studio/whip", server.http_addr());
    if let Some(made) = push(&whip, Push::Whip, None) {
        assert!(made.is_err(), "WHIP");
    }
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(state_of(&server), SourceState::AwaitingPublisher);

    // A channel is pulled or pushed, one or the other.
    for body in [
        r#"{"name":"both","source":"rtsp://127.0.0.1:1/cam","publish":"rtmp"}"#,
        r#"{"name":"neither"}"#,
        r#"{"name":"other","publish":"srt"}"#,
    ] {
        let (status, _, _) = http(server.http_addr(), "POST", "/api/v1/channels", Some(body));
        assert_eq!(status, 400, "{body}");
    }
    server.stop();
}
