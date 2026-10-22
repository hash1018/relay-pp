//! The one HTTP port: the status API, and WHEP for WebRTC players — see
//! `docs/design.md`, "HTTP".
//!
//! WHEP is answered here over `WebRtcServerHandle::play` and `close` rather
//! than by `WebRtcServer::serve_http`'s listener of its own, so there is one
//! port to open and one place that decides who may play.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use media_pp::elements::{SessionId, WebRtcServerError, WebRtcServerHandle};

use crate::channel::{Channel, ChannelStatus};

/// Every channel by its name.
pub(crate) type Channels = Arc<RwLock<BTreeMap<String, Arc<Channel>>>>;

/// What the routes reach.
#[derive(Clone)]
pub(crate) struct AppState {
    /// Read on every request, written only as channels are added or
    /// removed: a handler takes the channel it needs out and lets go of the
    /// lock before doing anything that waits.
    pub(crate) channels: Channels,
    pub(crate) webrtc: WebRtcServerHandle,
}

/// Every route.
pub(crate) fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/v1/channels", get(list_channels))
        .route("/api/v1/channels/{*name}", get(one_channel))
        .route(
            "/{*rest}",
            axum::routing::post(whep_offer)
                .delete(whep_end)
                .options(whep_options),
        )
        .with_state(state)
}

/// The channel called `name`, the lock let go of.
fn channel(state: &AppState, name: &str) -> Option<Arc<Channel>> {
    state
        .channels
        .read()
        .unwrap_or_else(|p| p.into_inner())
        .get(name)
        .cloned()
}

async fn list_channels(State(state): State<AppState>) -> Response {
    let channels: Vec<Arc<Channel>> = state
        .channels
        .read()
        .unwrap_or_else(|p| p.into_inner())
        .values()
        .cloned()
        .collect();
    let channels: Vec<ChannelStatus> = channels.iter().map(|channel| channel.status()).collect();
    axum::Json(serde_json::json!({ "channels": channels })).into_response()
}

async fn one_channel(State(state): State<AppState>, Path(name): Path<String>) -> Response {
    match channel(&state, &name) {
        Some(channel) => axum::Json(channel.status()).into_response(),
        None => (StatusCode::NOT_FOUND, format!("no channel {name}\n")).into_response(),
    }
}

/// `POST /{channel}/whep` with an SDP offer: a session playing the channel,
/// and its answer.
async fn whep_offer(
    State(state): State<AppState>,
    Path(rest): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(name) = rest.strip_suffix("/whep").map(str::to_owned) else {
        return StatusCode::NOT_FOUND.into_response();
    };
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

/// `DELETE /{channel}/whep/{session}`: the player leaves.
async fn whep_end(State(state): State<AppState>, Path(rest): Path<String>) -> Response {
    let session = rest
        .rsplit_once("/whep/")
        .and_then(|(_, session)| session.parse::<SessionId>().ok());
    let Some(session) = session else {
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
async fn whep_options(Path(rest): Path<String>) -> Response {
    if !rest.ends_with("/whep") {
        return StatusCode::NOT_FOUND.into_response();
    }
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
