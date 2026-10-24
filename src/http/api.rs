//! The manager's API — channels made, removed, recorded, their status and
//! the metrics — and the player's: where an RTSP player plays a channel,
//! and what a channel recorded.

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use super::{AppState, attach_refusal, found, playable, unplayable};
use crate::channel::{AttachError, ChannelConfig, ChannelError, ChannelStatus, Output};
use crate::{clock, metrics, recordings};

pub(super) async fn list_channels(State(state): State<AppState>) -> Response {
    let channels: Vec<ChannelStatus> = state
        .channels
        .list()
        .iter()
        .map(|channel| channel.status())
        .collect();
    axum::Json(serde_json::json!({ "channels": channels })).into_response()
}

/// `GET /metrics`: see [`metrics`].
pub(super) async fn metrics(State(state): State<AppState>) -> Response {
    let readings = metrics::Readings {
        channels: state
            .channels
            .list()
            .iter()
            .map(|channel| channel.status())
            .collect(),
        rtsp_players: state.rtsp.players(),
        webrtc_sessions: state.webrtc.session_stats(),
        recorded_bytes: *state
            .recorded_bytes
            .lock()
            .unwrap_or_else(|p| p.into_inner()),
    };
    (
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        metrics::text(&readings),
    )
        .into_response()
}

pub(super) async fn one_channel(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Response {
    match state.channels.get(&name) {
        Some(channel) => axum::Json(channel.status()).into_response(),
        None => (StatusCode::NOT_FOUND, format!("no channel {name}\n")).into_response(),
    }
}

/// `POST /api/v1/channels` with a [`ChannelConfig`]: the channel made, its
/// camera opened, and its status.
pub(super) async fn create_channel(
    State(state): State<AppState>,
    body: Result<axum::Json<ChannelConfig>, JsonRejection>,
) -> Response {
    let config = match body {
        Ok(axum::Json(config)) => config,
        Err(rejection) => {
            return (
                StatusCode::BAD_REQUEST,
                format!("{}\n", rejection.body_text()),
            )
                .into_response();
        }
    };
    match state.channels.add(&config).map(|channel| channel.status()) {
        Ok(status) => {
            let mut response = (StatusCode::CREATED, axum::Json(&status)).into_response();
            if let Ok(location) =
                HeaderValue::from_str(&format!("/api/v1/channels/{}", status.name))
            {
                response.headers_mut().insert(header::LOCATION, location);
            }
            response
        }
        Err(error) => {
            let status = match error {
                ChannelError::Name(_) | ChannelError::Source { .. } | ChannelError::Input(_) => {
                    StatusCode::BAD_REQUEST
                }
                ChannelError::Intake { .. } => StatusCode::CONFLICT,
                ChannelError::Exists(_) => StatusCode::CONFLICT,
            };
            (status, format!("{error}\n")).into_response()
        }
    }
}

/// `DELETE /api/v1/channels/{name}`: answered once the channel is off every
/// listener, its recording finished and its camera let go of.
pub(super) async fn delete_channel(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Response {
    if state.channels.remove(&name).await {
        StatusCode::NO_CONTENT.into_response()
    } else {
        StatusCode::NOT_FOUND.into_response()
    }
}

/// The body of `POST /api/v1/channels/{name}/play`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PlayRequest {
    protocol: String,
}

/// `POST /api/v1/channels/{name}/play` with `{"protocol":"rtsp"}`: the
/// channel registered on the RTSP listener where it was not yet, its camera
/// waited for until it is live, and the URL an RTSP player opens it at.
///
/// RTSP players connect to the RTSP port and not here, so what is to play
/// a channel over RTSP asks here first. A WebRTC player needs no such
/// question: its offer to `/{name}/whep` is one.
pub(super) async fn play(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    body: Result<axum::Json<PlayRequest>, JsonRejection>,
) -> Response {
    let request = match body {
        Ok(axum::Json(request)) => request,
        Err(rejection) => {
            return (
                StatusCode::BAD_REQUEST,
                format!("{}\n", rejection.body_text()),
            )
                .into_response();
        }
    };
    match request.protocol.as_str() {
        "rtsp" => {}
        "webrtc" => {
            return (
                StatusCode::BAD_REQUEST,
                format!("a WebRTC player offers to /{name}/whep itself\n"),
            )
                .into_response();
        }
        other => {
            return (
                StatusCode::BAD_REQUEST,
                format!("{other:?} is not a protocol played here: \"rtsp\"\n"),
            )
                .into_response();
        }
    }
    let channel = match found(&state, &name) {
        Ok(channel) => channel,
        Err(refused) => return refused.into_response(),
    };
    if let Err(error) = playable(&state, &channel, Output::Rtsp).await {
        return unplayable(&error);
    }
    let url = match &state.rtsp_public_url {
        Some(public) => format!("{}/{name}", public.trim_end_matches('/')),
        None => format!("rtsp://{}:{}/{name}", host(&headers), state.rtsp_port),
    };
    axum::Json(serde_json::json!({ "url": url })).into_response()
}

/// The host a request came to, without its port: what a player on the
/// same side of the network reaches this machine by.
fn host(headers: &HeaderMap) -> String {
    let host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("localhost");
    match host.strip_prefix('[') {
        // An IPv6 address, bracketed.
        Some(rest) => format!("[{}]", rest.split(']').next().unwrap_or_default()),
        None => host.split(':').next().unwrap_or(host).to_owned(),
    }
}

/// `POST /api/v1/channels/{name}/record`: the channel recorded from now —
/// from the newest keyframe it holds, where it is live — until told not to.
pub(super) async fn record(State(state): State<AppState>, Path(name): Path<String>) -> Response {
    match state.channels.record(&name) {
        Ok(()) => StatusCode::CREATED.into_response(),
        Err(error) => attach_refused(&error),
    }
}

/// `DELETE /api/v1/channels/{name}/record`: answered once the recording
/// under way is finished, its playlist ended.
pub(super) async fn unrecord(State(state): State<AppState>, Path(name): Path<String>) -> Response {
    match state.channels.unrecord(&name).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => attach_refused(&error),
    }
}

fn attach_refused(error: &AttachError) -> Response {
    attach_refusal(error).into_response()
}

/// `GET /api/v1/recordings/{name}`: a channel's recordings, oldest first —
/// whether or not the channel is there now.
pub(super) async fn list_recordings(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Response {
    let root = state.channels.record.dir.clone();
    let listed = tokio::task::spawn_blocking(move || {
        recordings::chunks(&root, &name)
            .into_iter()
            .map(|chunk| {
                let playlist = chunk.playlist().unwrap_or_default();
                let (bytes, _) = chunk.size_and_written();
                serde_json::json!({
                    "start": clock::rfc3339(chunk.start),
                    "duration_s": playlist.duration().as_secs_f64(),
                    "bytes": bytes,
                    "recording": !playlist.ended,
                    "playlist": format!("{}/{}", chunk.url, recordings::PLAYLIST),
                })
            })
            .collect::<Vec<_>>()
    })
    .await;
    match listed {
        Ok(listed) => axum::Json(serde_json::json!({ "recordings": listed })).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_rtsp_url_takes_the_host_a_request_came_to() {
        let with = |host: &str| {
            let mut headers = HeaderMap::new();
            headers.insert(header::HOST, HeaderValue::from_str(host).unwrap());
            super::host(&headers)
        };
        assert_eq!(with("192.168.0.10:8888"), "192.168.0.10");
        assert_eq!(with("cctv.example.com"), "cctv.example.com");
        assert_eq!(with("[::1]:8888"), "[::1]");
        assert_eq!(super::host(&HeaderMap::new()), "localhost");
    }
}
