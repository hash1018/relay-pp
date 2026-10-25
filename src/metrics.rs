//! `GET /metrics`: what the server is doing, in Prometheus's text format —
//! each channel's camera, players, recording and conversions, how its
//! WebRTC sessions are doing, and the recordings' size on disk.
//!
//! Read at the moment it is asked for, from what the channels and the
//! listeners already count; nothing is counted for it. A channel's name is
//! one part of a URL's path — letters, digits, `-`, `_`, `.`, `~` — so it
//! goes into a label as it is.

use std::collections::BTreeMap;
use std::fmt::Write;

use media_pp::elements::{SessionRole, SessionStats};

use crate::channel::{ChannelStatus, SourceState};

/// What one answer is made of.
pub(crate) struct Readings {
    pub(crate) channels: Vec<ChannelStatus>,
    /// RTSP players playing now, every channel together: the listener
    /// counts no more finely.
    pub(crate) rtsp_players: usize,
    pub(crate) webrtc_sessions: Vec<WebRtcSession>,
    /// What every recording takes on disk, as the last sweep measured it —
    /// none before the first.
    pub(crate) recorded_bytes: Option<u64>,
}

/// What the metrics read of one of the WebRTC server's sessions open now.
pub(crate) struct WebRtcSession {
    pub(crate) path: String,
    pub(crate) role: SessionRole,
    /// The share of its packets lost over the last second or so, where its
    /// connection has measured it.
    pub(crate) loss: Option<f32>,
    /// Packets asked to be sent again since it opened: by a player of the
    /// server, or by the server of a publisher.
    pub(crate) resend_requests: u64,
    /// Times a player fell so far behind that it went on from a keyframe.
    pub(crate) fell_behind: u64,
}

impl From<&SessionStats> for WebRtcSession {
    fn from(stats: &SessionStats) -> Self {
        Self {
            path: stats.path.clone(),
            role: stats.role,
            loss: stats.loss,
            resend_requests: stats.resend_requests,
            fell_behind: stats.fell_behind,
        }
    }
}

/// Each role a WebRTC session has, by the name its label gives it.
const ROLES: [(SessionRole, &str); 2] = [
    (SessionRole::Play, "play"),
    (SessionRole::Publish, "publish"),
];

/// Every state a channel's source is in, by the name the API gives it.
const STATES: [(SourceState, &str); 4] = [
    (SourceState::Connecting, "connecting"),
    (SourceState::Live, "live"),
    (SourceState::Waiting, "waiting"),
    (SourceState::AwaitingPublisher, "awaiting_publisher"),
];

/// The answer, in Prometheus's text format.
pub(crate) fn text(readings: &Readings) -> String {
    let mut out = String::new();
    let channels = &readings.channels;
    let mut family = |name: &str, kind: &str, help: &str, samples: &[(String, f64)]| {
        let _ = writeln!(out, "# HELP {name} {help}");
        let _ = writeln!(out, "# TYPE {name} {kind}");
        for (labels, value) in samples {
            let _ = writeln!(out, "{name}{labels} {value}");
        }
    };
    let per_channel = |value: &dyn Fn(&ChannelStatus) -> f64| -> Vec<(String, f64)> {
        channels
            .iter()
            .map(|status| (format!("{{channel=\"{}\"}}", status.name), value(status)))
            .collect()
    };
    let flag = |yes: bool| if yes { 1.0 } else { 0.0 };

    family(
        "relay_pp_channel_up",
        "gauge",
        "Whether the channel's stream is coming in.",
        &per_channel(&|status| flag(status.state == SourceState::Live)),
    );
    let states: Vec<(String, f64)> = channels
        .iter()
        .flat_map(|status| {
            STATES.iter().map(move |(state, name)| {
                (
                    format!("{{channel=\"{}\",state=\"{name}\"}}", status.name),
                    flag(status.state == *state),
                )
            })
        })
        .collect();
    family(
        "relay_pp_channel_state",
        "gauge",
        "What the channel's source is doing: 1 for the state it is in.",
        &states,
    );
    family(
        "relay_pp_channel_connections_total",
        "counter",
        "How many times the channel's source has gone live.",
        &per_channel(&|status| status.connections as f64),
    );
    family(
        "relay_pp_channel_received_bytes_total",
        "counter",
        "What the channel's source has written, over every connection.",
        &per_channel(&|status| status.bytes as f64),
    );
    family(
        "relay_pp_channel_readers",
        "gauge",
        "Readers of the channel's stream: players, its recording, its HLS, its conversions.",
        &per_channel(&|status| status.readers as f64),
    );
    family(
        "relay_pp_channel_recording",
        "gauge",
        "Whether a recording of the channel is being written.",
        &per_channel(&|status| flag(status.recording.is_some())),
    );
    family(
        "relay_pp_channel_record_failed",
        "gauge",
        "Whether the channel's last recording failed, until one is written again.",
        &per_channel(&|status| flag(status.record_error.is_some())),
    );
    family(
        "relay_pp_channel_hls",
        "gauge",
        "Whether the channel's HLS is being written, for players asking for it.",
        &per_channel(&|status| flag(status.sinks.contains(&"hls"))),
    );
    family(
        "relay_pp_channel_conversions",
        "gauge",
        "Tracks of the channel converted now.",
        &per_channel(&|status| status.conversions.len() as f64),
    );
    family(
        "relay_pp_channel_gpu_refused_total",
        "counter",
        "Times the channel's pictures were refused a conversion for want of a free GPU encoder.",
        &per_channel(&|status| status.gpu_refused as f64),
    );

    let mut webrtc: BTreeMap<&str, usize> = channels
        .iter()
        .map(|status| (status.name.as_str(), 0))
        .collect();
    for session in &readings.webrtc_sessions {
        if session.role == SessionRole::Play
            && let Some(players) = webrtc.get_mut(session.path.as_str())
        {
            *players += 1;
        }
    }
    let webrtc: Vec<(String, f64)> = webrtc
        .into_iter()
        .map(|(channel, players)| (format!("{{channel=\"{channel}\"}}"), players as f64))
        .collect();
    family(
        "relay_pp_channel_webrtc_players",
        "gauge",
        "WebRTC players of the channel.",
        &webrtc,
    );
    // How the channel's WebRTC sessions open now are doing: what each has
    // counted since it opened, together, and the worst loss among them.
    let sessions = |channel: &str, role: SessionRole| -> Vec<&WebRtcSession> {
        readings
            .webrtc_sessions
            .iter()
            .filter(|session| session.path == channel && session.role == role)
            .collect()
    };
    let per_role = |value: &dyn Fn(&str, SessionRole) -> Option<f64>| -> Vec<(String, f64)> {
        channels
            .iter()
            .flat_map(|status| {
                ROLES.iter().filter_map(|(role, name)| {
                    value(&status.name, *role).map(|value| {
                        (
                            format!("{{channel=\"{}\",role=\"{name}\"}}", status.name),
                            value,
                        )
                    })
                })
            })
            .collect()
    };
    family(
        "relay_pp_channel_webrtc_resend_requests",
        "gauge",
        "Packets the channel's WebRTC sessions open now asked to be sent again, by players of the server or by the server of the publisher: lost on the way.",
        &per_role(&|channel, role| {
            Some(
                sessions(channel, role)
                    .into_iter()
                    .map(|session| session.resend_requests)
                    .sum::<u64>() as f64,
            )
        }),
    );
    family(
        "relay_pp_channel_webrtc_loss",
        "gauge",
        "The largest share of packets lost, from 0 to 1, over the last second among the channel's WebRTC sessions open now; absent where none has measured it.",
        &per_role(&|channel, role| {
            sessions(channel, role)
                .into_iter()
                .filter_map(|session| session.loss)
                .map(f64::from)
                .reduce(f64::max)
        }),
    );
    family(
        "relay_pp_channel_webrtc_fell_behind",
        "gauge",
        "Times the channel's WebRTC players open now fell so far behind that they went on from a keyframe, skipping pictures.",
        &per_channel(&|status| {
            sessions(&status.name, SessionRole::Play)
                .into_iter()
                .map(|session| session.fell_behind)
                .sum::<u64>() as f64
        }),
    );
    family(
        "relay_pp_channel_rtsp_players",
        "gauge",
        "RTSP players of the channel.",
        &per_channel(&|status| status.rtsp_players as f64),
    );
    family(
        "relay_pp_rtsp_players",
        "gauge",
        "RTSP players, every channel together.",
        &[(String::new(), readings.rtsp_players as f64)],
    );
    if let Some(bytes) = readings.recorded_bytes {
        family(
            "relay_pp_recordings_bytes",
            "gauge",
            "What every recording takes on disk, as last measured — once a minute.",
            &[(String::new(), bytes as f64)],
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ConvertConfig;

    fn status(name: &str, state: SourceState) -> ChannelStatus {
        ChannelStatus {
            name: name.to_owned(),
            source: "rtsp://10.0.0.1/".to_owned(),
            state,
            since_ms: 0,
            error: None,
            connections: 3,
            client: Some("rtsp"),
            readers: 2,
            rtsp_players: 1,
            webrtc_players: 0,
            bytes: 1_000_000,
            sinks: vec!["webrtc", "record", "hls"],
            recording: Some("/recordings/x/index.m3u8".to_owned()),
            record_error: None,
            convert: ConvertConfig::default(),
            conversions: vec!["1:opus".to_owned()],
            gpu_refused: 4,
        }
    }

    #[test]
    fn each_channel_is_read_out_by_its_name() {
        let text = text(&Readings {
            channels: vec![
                status("cam1", SourceState::Live),
                status("cam2", SourceState::Waiting),
            ],
            rtsp_players: 5,
            webrtc_sessions: vec![],
            recorded_bytes: Some(123),
        });
        for line in [
            "# TYPE relay_pp_channel_up gauge",
            r#"relay_pp_channel_up{channel="cam1"} 1"#,
            r#"relay_pp_channel_up{channel="cam2"} 0"#,
            r#"relay_pp_channel_state{channel="cam2",state="waiting"} 1"#,
            r#"relay_pp_channel_state{channel="cam2",state="live"} 0"#,
            "# TYPE relay_pp_channel_connections_total counter",
            r#"relay_pp_channel_connections_total{channel="cam1"} 3"#,
            r#"relay_pp_channel_received_bytes_total{channel="cam1"} 1000000"#,
            r#"relay_pp_channel_recording{channel="cam1"} 1"#,
            r#"relay_pp_channel_record_failed{channel="cam1"} 0"#,
            r#"relay_pp_channel_hls{channel="cam1"} 1"#,
            r#"relay_pp_channel_conversions{channel="cam1"} 1"#,
            r#"relay_pp_channel_gpu_refused_total{channel="cam1"} 4"#,
            r#"relay_pp_channel_webrtc_players{channel="cam1"} 0"#,
            "relay_pp_rtsp_players 5",
            "relay_pp_recordings_bytes 123",
        ] {
            assert!(text.lines().any(|got| got == line), "{line}\n---\n{text}");
        }
    }

    fn session(path: &str, role: SessionRole, loss: Option<f32>, resends: u64) -> WebRtcSession {
        WebRtcSession {
            path: path.to_owned(),
            role,
            loss,
            resend_requests: resends,
            fell_behind: resends / 2,
        }
    }

    /// The sessions open now, read out by their channel and role: resends
    /// and skips together, the worst loss, and no loss where none was
    /// measured.
    #[test]
    fn a_channel_s_webrtc_sessions_say_what_they_lost() {
        let text = text(&Readings {
            channels: vec![
                status("cam1", SourceState::Live),
                status("cam2", SourceState::Live),
            ],
            rtsp_players: 0,
            webrtc_sessions: vec![
                session("cam1", SessionRole::Play, Some(0.02), 4),
                session("cam1", SessionRole::Play, Some(0.25), 6),
                session("cam1", SessionRole::Play, None, 0),
                session("cam1", SessionRole::Publish, Some(0.0), 3),
                session("elsewhere", SessionRole::Play, Some(0.9), 100),
            ],
            recorded_bytes: None,
        });
        for line in [
            "# TYPE relay_pp_channel_webrtc_resend_requests gauge",
            r#"relay_pp_channel_webrtc_resend_requests{channel="cam1",role="play"} 10"#,
            r#"relay_pp_channel_webrtc_resend_requests{channel="cam1",role="publish"} 3"#,
            r#"relay_pp_channel_webrtc_resend_requests{channel="cam2",role="play"} 0"#,
            r#"relay_pp_channel_webrtc_loss{channel="cam1",role="play"} 0.25"#,
            r#"relay_pp_channel_webrtc_loss{channel="cam1",role="publish"} 0"#,
            r#"relay_pp_channel_webrtc_fell_behind{channel="cam1"} 5"#,
            r#"relay_pp_channel_webrtc_fell_behind{channel="cam2"} 0"#,
            r#"relay_pp_channel_webrtc_players{channel="cam1"} 3"#,
        ] {
            assert!(text.lines().any(|got| got == line), "{line}\n---\n{text}");
        }
        assert!(
            !text.contains(r#"relay_pp_channel_webrtc_loss{channel="cam2""#),
            "no loss where none was measured\n{text}"
        );
        assert!(!text.contains("elsewhere"), "only channels\n{text}");
    }

    #[test]
    fn the_recordings_size_is_left_out_until_it_is_measured() {
        let text = text(&Readings {
            channels: vec![],
            rtsp_players: 0,
            webrtc_sessions: vec![],
            recorded_bytes: None,
        });
        assert!(!text.contains("relay_pp_recordings_bytes"), "{text}");
        assert!(text.contains("relay_pp_rtsp_players 0"), "{text}");
    }
}
