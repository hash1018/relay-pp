//! WebRTC's offers: WHEP for a player, WHIP for a publisher, each answered
//! over the WebRTC listener, and their sessions ended.

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use media_pp::elements::{SessionId, WebRtcServerError};

use super::{AppState, found, playable, unplayable};
use crate::channel::{Output, whip_path};

/// `POST /{name}/whep` with an SDP offer: the channel registered on the
/// WebRTC listener where it was not yet, its camera waited for until it is
/// live, and a session playing it, with its answer.
pub(super) async fn whep_offer(
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
    let channel = match found(&state, &name) {
        Ok(channel) => channel,
        Err(refused) => return refused.into_response(),
    };
    if let Err(error) = playable(&state, &channel, Output::WebRtc).await {
        return unplayable(&error);
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
pub(super) async fn whep_end(
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

/// `POST /{name}/whip` with an SDP offer: a publisher of a channel made to
/// take one over WHIP, and the answer — `404` for a channel that takes none
/// so, `409` while another publishes.
pub(super) async fn whip_offer(
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
    let webrtc = state.webrtc.clone();
    let path = whip_path(&name);
    match tokio::task::spawn_blocking(move || webrtc.publish(&path, &offer)).await {
        Ok(Ok((session, answer))) => {
            tracing::info!(channel = %name, "WHIP publisher {session} joined");
            let mut response = (StatusCode::CREATED, answer).into_response();
            let headers = response.headers_mut();
            headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/sdp"),
            );
            if let Ok(location) = HeaderValue::from_str(&format!("/{name}/whip/{session}")) {
                headers.insert(header::LOCATION, location);
            }
            response
        }
        Ok(Err(WebRtcServerError::NoSuchPath(_))) => (
            StatusCode::NOT_FOUND,
            format!("no channel {name} takes a WHIP publisher\n"),
        )
            .into_response(),
        Ok(Err(error)) => refused(&error),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// `OPTIONS` on a WHIP URL: what it takes.
pub(super) async fn whip_options() -> Response {
    let mut response = StatusCode::NO_CONTENT.into_response();
    let headers = response.headers_mut();
    headers.insert(header::ALLOW, HeaderValue::from_static("OPTIONS, POST"));
    headers.insert("accept-post", HeaderValue::from_static("application/sdp"));
    response
}

/// `OPTIONS` on a WHEP URL: what it takes.
pub(super) async fn whep_options() -> Response {
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
        // Another publisher publishes into it.
        WebRtcServerError::NotPublishable(_) => StatusCode::CONFLICT,
        WebRtcServerError::NothingToPlay(_) => StatusCode::SERVICE_UNAVAILABLE,
        WebRtcServerError::InvalidOffer(_) => StatusCode::BAD_REQUEST,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (status, format!("{error}\n")).into_response()
}
