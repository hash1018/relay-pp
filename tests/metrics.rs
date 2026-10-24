//! `GET /metrics` against what is running: a camera pulled, recorded, and
//! played over RTSP and WebRTC, read out as Prometheus reads it.

mod common;

use common::*;
use relay_pp::SourceState;

/// The value of the sample `series` — a name and its labels, as written —
/// in `metrics`.
fn value(metrics: &str, series: &str) -> Option<f64> {
    metrics.lines().find_map(|line| {
        line.strip_prefix(series)
            .and_then(|rest| rest.strip_prefix(' '))
            .and_then(|value| value.parse().ok())
    })
}

#[test]
fn what_runs_is_read_out() {
    let camera = Camera::start("127.0.0.1:0".parse().unwrap());
    let root = scratch("metrics");
    let server = server_with(&format!(
        "[record]\ndir = {:?}\nsegment = \"1s\"",
        root.display().to_string()
    ));
    let api = server.http_addr();
    let body = channel("cam1", &format!("rtsp://{}/cam", camera.addr));
    assert_eq!(http(api, "POST", "/api/v1/channels", Some(&body)).0, 201);
    wait_for("the camera live", || state_of(&server) == SourceState::Live);
    assert_eq!(
        http(api, "POST", "/api/v1/channels/cam1/record", None).0,
        201
    );
    let (rtsp, _) = rtsp_player(&play_rtsp(api, "cam1"));
    let (webrtc, _) = whep_player(&format!("http://{api}/cam1/whep"));

    let read = || {
        let (status, head, metrics) = http(api, "GET", "/metrics", None);
        assert_eq!(status, 200);
        assert!(head.contains("content-type: text/plain"), "{head}");
        metrics
    };
    wait_for("the players and the recording counted", || {
        let metrics = read();
        value(
            &metrics,
            r#"relay_pp_channel_webrtc_players{channel="cam1"}"#,
        ) == Some(1.0)
            && value(&metrics, "relay_pp_rtsp_players") == Some(1.0)
            && value(&metrics, r#"relay_pp_channel_recording{channel="cam1"}"#) == Some(1.0)
    });
    let metrics = read();
    for (series, expected) in [
        (r#"relay_pp_channel_up{channel="cam1"}"#, 1.0),
        (
            r#"relay_pp_channel_state{channel="cam1",state="live"}"#,
            1.0,
        ),
        (
            r#"relay_pp_channel_state{channel="cam1",state="waiting"}"#,
            0.0,
        ),
        (r#"relay_pp_channel_connections_total{channel="cam1"}"#, 1.0),
        (r#"relay_pp_channel_record_failed{channel="cam1"}"#, 0.0),
        (r#"relay_pp_channel_gpu_refused_total{channel="cam1"}"#, 0.0),
    ] {
        assert_eq!(
            value(&metrics, series),
            Some(expected),
            "{series}\n{metrics}"
        );
    }
    assert!(
        value(
            &metrics,
            r#"relay_pp_channel_received_bytes_total{channel="cam1"}"#
        )
        .is_some_and(|bytes| bytes > 0.0),
        "{metrics}"
    );
    assert!(
        value(&metrics, r#"relay_pp_channel_readers{channel="cam1"}"#)
            .is_some_and(|readers| readers >= 3.0),
        "two players and a recording: {metrics}"
    );
    // Measured by the sweep at the start, every minute after.
    assert!(
        value(&metrics, "relay_pp_recordings_bytes").is_some(),
        "{metrics}"
    );

    rtsp.stop();
    webrtc.stop();
    server.stop();
    camera.stop();
    let _ = std::fs::remove_dir_all(&root);
}
