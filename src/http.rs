//! The one HTTP port: the API, WHEP for WebRTC players, and recordings — see
//! `docs/design.md`, "HTTP".
//!
//! WHEP is answered here over `WebRtcServerHandle::play` and `close` rather
//! than by `WebRtcServer::serve_http`'s listener of its own, so there is one
//! port to open and one place that decides who may play.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use axum::Router;
use axum::body::Bytes;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use media_pp::elements::{SessionId, WebRtcServerError, WebRtcServerHandle};
use serde::Deserialize;
use tower_http::services::ServeDir;

use crate::channel::{AttachError, Channel, ChannelConfig, ChannelError, ChannelStatus, Protocol};
use crate::channels::Channels;
use crate::{clock, recordings};

/// How long a player's request waits for a channel's camera to be live —
/// one just made, or connecting again — before it is answered that it is
/// not.
const LIVE_WITHIN: Duration = Duration::from_secs(10);

/// What the routes reach.
#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) channels: Arc<Channels>,
    pub(crate) webrtc: WebRtcServerHandle,
    /// The port RTSP players connect to.
    pub(crate) rtsp_port: u16,
    /// What RTSP URLs begin with in place of the host a request came to
    /// and `rtsp_port`.
    pub(crate) rtsp_public_url: Option<String>,
}

/// Every route.
pub(crate) fn router(state: AppState) -> Router {
    // A recording's files as they are: its own playlist — which a player
    // follows while it is written — and the byte ranges of its media.
    let files = ServeDir::new(&state.channels.record.dir);
    Router::new()
        .route("/api/v1/channels", get(list_channels).post(create_channel))
        .route(
            "/api/v1/channels/{name}",
            get(one_channel).delete(delete_channel),
        )
        .route("/api/v1/channels/{name}/play", post(play))
        .route(
            "/api/v1/channels/{name}/record",
            post(record).delete(unrecord),
        )
        .route("/api/v1/recordings/{name}", get(list_recordings))
        .route("/playback/{name}/index.m3u8", get(playback))
        .nest_service(recordings::URL_ROOT, files)
        .route("/{name}/whep", post(whep_offer).options(whep_options))
        .route("/{name}/whep/{session}", axum::routing::delete(whep_end))
        .with_state(state)
}

async fn list_channels(State(state): State<AppState>) -> Response {
    let channels: Vec<ChannelStatus> = state
        .channels
        .list()
        .iter()
        .map(|channel| channel.status())
        .collect();
    axum::Json(serde_json::json!({ "channels": channels })).into_response()
}

async fn one_channel(State(state): State<AppState>, Path(name): Path<String>) -> Response {
    match state.channels.get(&name) {
        Some(channel) => axum::Json(channel.status()).into_response(),
        None => (StatusCode::NOT_FOUND, format!("no channel {name}\n")).into_response(),
    }
}

/// `POST /api/v1/channels` with a [`ChannelConfig`]: the channel made, its
/// camera opened, and its status.
async fn create_channel(
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
    let channels = Arc::clone(&state.channels);
    let added =
        tokio::task::spawn_blocking(move || channels.add(&config).map(|channel| channel.status()))
            .await;
    match added {
        Ok(Ok(status)) => {
            let mut response = (StatusCode::CREATED, axum::Json(&status)).into_response();
            if let Ok(location) =
                HeaderValue::from_str(&format!("/api/v1/channels/{}", status.name))
            {
                response.headers_mut().insert(header::LOCATION, location);
            }
            response
        }
        Ok(Err(error)) => {
            let status = match error {
                ChannelError::Name(_) | ChannelError::Source { .. } => StatusCode::BAD_REQUEST,
                ChannelError::Exists(_) => StatusCode::CONFLICT,
            };
            (status, format!("{error}\n")).into_response()
        }
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// `DELETE /api/v1/channels/{name}`: answered once the channel is off every
/// listener, its recording finished and its camera let go of.
async fn delete_channel(State(state): State<AppState>, Path(name): Path<String>) -> Response {
    let channels = Arc::clone(&state.channels);
    match tokio::task::spawn_blocking(move || channels.remove(&name)).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => StatusCode::NOT_FOUND.into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// The body of `POST /api/v1/channels/{name}/play`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PlayRequest {
    protocol: String,
}

/// `POST /api/v1/channels/{name}/play` with `{"protocol":"rtsp"}`: the
/// channel registered on the RTSP listener where it was not yet, its camera
/// waited for until it is live, and the URL an RTSP player opens it at.
///
/// RTSP players connect to the RTSP port and not here, so what is to play
/// a channel over RTSP asks here first. A WebRTC player needs no such
/// question: its offer to `/{name}/whep` is one.
async fn play(
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
    let channel = match streamed(&state, &name, Protocol::Rtsp).await {
        Ok(channel) => channel,
        Err(refused) => return refused.into_response(),
    };
    if let Err(reason) = channel.live(LIVE_WITHIN).await {
        return (StatusCode::SERVICE_UNAVAILABLE, format!("{reason}\n")).into_response();
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

/// The channel called `name`, registered on `protocol`'s listener — or the
/// answer saying why not.
async fn streamed(
    state: &AppState,
    name: &str,
    protocol: Protocol,
) -> Result<Arc<Channel>, (StatusCode, String)> {
    let channels = Arc::clone(&state.channels);
    let name = name.to_owned();
    match tokio::task::spawn_blocking(move || channels.stream(&name, protocol)).await {
        Ok(Ok(channel)) => Ok(channel),
        Ok(Err(error)) => Err(attach_refusal(&error)),
        Err(_) => Err((StatusCode::INTERNAL_SERVER_ERROR, String::new())),
    }
}

/// `POST /api/v1/channels/{name}/record`: the channel recorded from now —
/// from the newest keyframe it holds, where it is live — until told not to.
async fn record(State(state): State<AppState>, Path(name): Path<String>) -> Response {
    match state.channels.record(&name) {
        Ok(()) => StatusCode::CREATED.into_response(),
        Err(error) => attach_refused(&error),
    }
}

/// `DELETE /api/v1/channels/{name}/record`: answered once the recording
/// under way is finished, its playlist ended.
async fn unrecord(State(state): State<AppState>, Path(name): Path<String>) -> Response {
    let channels = Arc::clone(&state.channels);
    match tokio::task::spawn_blocking(move || channels.unrecord(&name)).await {
        Ok(Ok(())) => StatusCode::NO_CONTENT.into_response(),
        Ok(Err(error)) => attach_refused(&error),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

fn attach_refused(error: &AttachError) -> Response {
    attach_refusal(error).into_response()
}

fn attach_refusal(error: &AttachError) -> (StatusCode, String) {
    let status = match error {
        AttachError::NoChannel(_) | AttachError::NotAttached(_) => StatusCode::NOT_FOUND,
        AttachError::Attached(_) => StatusCode::CONFLICT,
        AttachError::Listener { .. } => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (status, format!("{error}\n"))
}

/// `GET /api/v1/recordings/{name}`: a channel's recordings, oldest first —
/// whether or not the channel is there now.
async fn list_recordings(State(state): State<AppState>, Path(name): Path<String>) -> Response {
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

/// `GET /playback/{name}/index.m3u8?from=…&to=…`: a playlist of what a
/// channel recorded between two RFC 3339 times in UTC — from its first
/// recording, and to now, where either is left out.
async fn playback(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let bound = |key: &str, otherwise: SystemTime| match query.get(key) {
        Some(text) => clock::parse_rfc3339(text).ok_or_else(|| {
            (
                StatusCode::BAD_REQUEST,
                format!("{key}: {text:?} is not a time like 2026-10-23T14:00:00Z\n"),
            )
        }),
        None => Ok(otherwise),
    };
    let (from, to) = match (
        bound("from", SystemTime::UNIX_EPOCH),
        bound("to", SystemTime::now()),
    ) {
        (Ok(from), Ok(to)) => (from, to),
        (Err(refused), _) | (_, Err(refused)) => return refused.into_response(),
    };
    let root = state.channels.record.dir.clone();
    let made = tokio::task::spawn_blocking(move || {
        let chunks: Vec<_> = recordings::chunks(&root, &name)
            .into_iter()
            .filter_map(|chunk| chunk.playlist().map(|playlist| (chunk, playlist)))
            .collect();
        recordings::playlist_between(&chunks, from, to)
    })
    .await;
    match made {
        Ok(Some(playlist)) => (
            [(header::CONTENT_TYPE, "application/vnd.apple.mpegurl")],
            playlist,
        )
            .into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "nothing recorded then\n").into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// `POST /{name}/whep` with an SDP offer: the channel registered on the
/// WebRTC listener where it was not yet, its camera waited for until it is
/// live, and a session playing it, with its answer.
async fn whep_offer(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let is_sdp = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("application/sdp"));
    if !is_sdp {
        return (
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "an offer is application/sdp\n",
        )
            .into_response();
    }
    let Ok(offer) = String::from_utf8(body.to_vec()) else {
        return (StatusCode::BAD_REQUEST, "the offer is not text\n").into_response();
    };
    let channel = match streamed(&state, &name, Protocol::WebRtc).await {
        Ok(channel) => channel,
        Err(refused) => return refused.into_response(),
    };
    if let Err(reason) = channel.live(LIVE_WITHIN).await {
        return (StatusCode::SERVICE_UNAVAILABLE, format!("{reason}\n")).into_response();
    }
    let webrtc = state.webrtc.clone();
    let played = {
        let name = name.clone();
        tokio::task::spawn_blocking(move || webrtc.play(&name, &offer)).await
    };
    match played {
        Ok(Ok((session, answer))) => {
            tracing::info!(channel = %name, "WebRTC player {session} joined");
            let mut response = (StatusCode::CREATED, answer).into_response();
            let headers = response.headers_mut();
            headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/sdp"),
            );
            if let Ok(location) = HeaderValue::from_str(&format!("/{name}/whep/{session}")) {
                headers.insert(header::LOCATION, location);
            }
            response
        }
        Ok(Err(error)) => refused(&error),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// `DELETE /{name}/whep/{session}`: the player leaves.
async fn whep_end(
    State(state): State<AppState>,
    Path((_, session)): Path<(String, String)>,
) -> Response {
    let Ok(session) = session.parse::<SessionId>() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let webrtc = state.webrtc.clone();
    match tokio::task::spawn_blocking(move || webrtc.close(session)).await {
        Ok(true) => StatusCode::OK.into_response(),
        Ok(false) => StatusCode::NOT_FOUND.into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// `OPTIONS` on a WHEP URL: what it takes.
async fn whep_options() -> Response {
    let mut response = StatusCode::NO_CONTENT.into_response();
    let headers = response.headers_mut();
    headers.insert(header::ALLOW, HeaderValue::from_static("OPTIONS, POST"));
    headers.insert("accept-post", HeaderValue::from_static("application/sdp"));
    response
}

/// An offer the server would not answer, as the status WHEP gives it.
fn refused(error: &WebRtcServerError) -> Response {
    let status = match error {
        WebRtcServerError::NoSuchPath(_) => StatusCode::NOT_FOUND,
        WebRtcServerError::NothingToPlay(_) => StatusCode::SERVICE_UNAVAILABLE,
        WebRtcServerError::InvalidOffer(_) => StatusCode::BAD_REQUEST,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (status, format!("{error}\n")).into_response()
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
