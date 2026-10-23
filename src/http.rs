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
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use media_pp::elements::{SessionId, WebRtcServerError, WebRtcServerHandle};
use serde::Deserialize;
use tower_http::services::ServeDir;

use crate::auth::{self, Auth, FromHeader, Presented, Right};
use crate::channel::{
    AttachError, Channel, ChannelConfig, ChannelError, ChannelStatus, Output, PlayError, Refusal,
};
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
    /// Who may do what; anybody anything, where `None`.
    pub(crate) auth: Option<Arc<Auth>>,
}

/// Every route: the manager's, a player's of a channel live, and a
/// watcher's of its recordings, each behind who may.
pub(crate) fn router(state: AppState) -> Router {
    let manager = Router::new()
        .route("/api/v1/channels", get(list_channels).post(create_channel))
        .route(
            "/api/v1/channels/{name}",
            get(one_channel).delete(delete_channel),
        )
        .route(
            "/api/v1/channels/{name}/record",
            post(record).delete(unrecord),
        )
        .route_layer(middleware::from_fn_with_state(state.clone(), manager_only));
    let players = Router::new()
        .route("/api/v1/channels/{name}/play", post(play))
        .route("/{name}/whep", post(whep_offer).options(whep_options))
        .route_layer(middleware::from_fn_with_state(
            (state.clone(), Right::Play),
            who_may,
        ));
    let watchers = Router::new()
        .route("/api/v1/recordings/{name}", get(list_recordings))
        .route("/playback/{name}/index.m3u8", get(playback))
        .route_layer(middleware::from_fn_with_state(
            (state.clone(), Right::Recordings),
            who_may,
        ));
    // A recording's files as they are: its own playlist — which a player
    // follows while it is written — and the byte ranges of its media.
    let files = Router::new()
        .fallback_service(ServeDir::new(&state.channels.record.dir))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            who_may_watch_files,
        ));
    Router::new()
        .merge(manager)
        .merge(players)
        .merge(watchers)
        .nest(recordings::URL_ROOT, files)
        // The session's own URL is the secret that ends it.
        .route("/{name}/whep/{session}", axum::routing::delete(whep_end))
        .with_state(state)
}

/// What a request presented: its `Authorization` header — a `Bearer` token
/// or a `Basic` user — or else a `?token=` in its URL, which a player that
/// sets no header carries.
struct Shown {
    token: Option<String>,
    user: Option<(String, String)>,
}

impl Shown {
    fn of(request: &Request) -> Self {
        let header = request
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(auth::from_header);
        match header {
            Some(FromHeader::Token(token)) => {
                return Self {
                    token: Some(token),
                    user: None,
                };
            }
            Some(FromHeader::User { name, password }) => {
                return Self {
                    token: None,
                    user: Some((name, password)),
                };
            }
            None => {}
        }
        Self {
            token: query_token(request.uri().query()),
            user: None,
        }
    }

    fn presented(&self) -> Presented<'_> {
        match (&self.token, &self.user) {
            (Some(token), _) => Presented::Token(token),
            (None, Some((user, password))) => Presented::Password { user, password },
            (None, None) => Presented::None,
        }
    }
}

/// The `token` a query carries.
fn query_token(query: Option<&str>) -> Option<String> {
    url::form_urlencoded::parse(query?.as_bytes())
        .find(|(key, _)| key == "token")
        .map(|(_, token)| token.into_owned())
}

/// Lets the manager alone through, where there is auth.
async fn manager_only(State(state): State<AppState>, request: Request, next: Next) -> Response {
    match &state.auth {
        Some(auth) if !auth.is_manager(&Shown::of(&request).presented()) => unauthorized(),
        _ => next.run(request).await,
    }
}

/// Lets through who may do `right` with the channel the route names, where
/// there is auth; a browser asking what WHEP takes, before it asks with its
/// credentials, always.
async fn who_may(
    State((state, right)): State<(AppState, Right)>,
    Path(params): Path<HashMap<String, String>>,
    request: Request,
    next: Next,
) -> Response {
    let Some(auth) = &state.auth else {
        return next.run(request).await;
    };
    let channel = params.get("name").map(String::as_str).unwrap_or_default();
    if request.method() == Method::OPTIONS
        || auth.allows(&Shown::of(&request).presented(), right, channel)
    {
        next.run(request).await
    } else {
        unauthorized()
    }
}

/// [`who_may`] watch the recordings of the channel a recording's file is
/// under: the first part of its path.
async fn who_may_watch_files(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let Some(auth) = &state.auth else {
        return next.run(request).await;
    };
    let channel = request
        .uri()
        .path()
        .trim_start_matches('/')
        .split('/')
        .next()
        .unwrap_or_default();
    if auth.allows(&Shown::of(&request).presented(), Right::Recordings, channel) {
        next.run(request).await
    } else {
        unauthorized()
    }
}

/// `401`, saying what is taken: a token, or a user's name and password.
fn unauthorized() -> Response {
    let mut response = (StatusCode::UNAUTHORIZED, "who are you?\n").into_response();
    let headers = response.headers_mut();
    headers.append(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
    headers.append(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static("Basic realm=\"relay-pp\""),
    );
    response
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
                ChannelError::Name(_) | ChannelError::Source { .. } => StatusCode::BAD_REQUEST,
                ChannelError::Exists(_) => StatusCode::CONFLICT,
            };
            (status, format!("{error}\n")).into_response()
        }
    }
}

/// `DELETE /api/v1/channels/{name}`: answered once the channel is off every
/// listener, its recording finished and its camera let go of.
async fn delete_channel(State(state): State<AppState>, Path(name): Path<String>) -> Response {
    if state.channels.remove(&name).await {
        StatusCode::NO_CONTENT.into_response()
    } else {
        StatusCode::NOT_FOUND.into_response()
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

/// The channel called `name` — or the answer saying there is none.
fn found(state: &AppState, name: &str) -> Result<Arc<Channel>, (StatusCode, String)> {
    state
        .channels
        .found(name)
        .map_err(|error| attach_refusal(&error))
}

/// Waits for `channel`'s camera to be live and what players over `output`
/// read to be made — or the answer saying why it is not.
async fn playable(state: &AppState, channel: &Channel, output: Output) -> Result<(), PlayError> {
    channel.live(LIVE_WITHIN).await?;
    state.channels.ready(channel, output, LIVE_WITHIN).await
}

/// The answer to a player `error` says cannot play.
fn unplayable(error: &PlayError) -> Response {
    let (status, retry) = answer_to(error);
    let mut response = (
        status,
        format!(
            "{error}
"
        ),
    )
        .into_response();
    if let Some(seconds) = retry {
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from(seconds));
    }
    response
}

/// The status a player who cannot play is answered with, and after how many
/// seconds to ask again where asking again may help: soon for a camera not
/// live yet, later for a GPU encoder none of which is free; never for a
/// channel that converts nothing it would need, nor a failure.
fn answer_to(error: &PlayError) -> (StatusCode, Option<u32>) {
    match error {
        PlayError::NoChannel(_) => (StatusCode::NOT_FOUND, None),
        PlayError::NotLive { .. } | PlayError::NotReady { .. } => {
            (StatusCode::SERVICE_UNAVAILABLE, Some(5))
        }
        PlayError::Refused { refusal, .. } => match refusal {
            Refusal::NotConverted { .. } => (StatusCode::CONFLICT, None),
            Refusal::NoGpu(_) => (StatusCode::SERVICE_UNAVAILABLE, Some(30)),
            Refusal::Failed(_) => (StatusCode::INTERNAL_SERVER_ERROR, None),
        },
        PlayError::Listener { .. } => (StatusCode::INTERNAL_SERVER_ERROR, None),
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
    match state.channels.unrecord(&name).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => attach_refused(&error),
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
            // A player that came with a token in its URL sets no header: each
            // segment it asks for next carries the token the same way.
            match (&state.auth, query.get("token")) {
                (Some(_), Some(token)) => with_token(&playlist, token),
                _ => playlist,
            },
        )
            .into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "nothing recorded then\n").into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// `playlist` with `token` in the URL of each media file it names: each
/// segment's line, and the `URI` of its initialization section.
fn with_token(playlist: &str, token: &str) -> String {
    let query: String = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("token", token)
        .finish();
    playlist
        .lines()
        .map(|line| {
            if line.starts_with(recordings::URL_ROOT) {
                format!("{line}?{query}")
            } else if let Some(at) = line.find(&format!("URI=\"{}", recordings::URL_ROOT)) {
                let closing = line[at + 5..].find('"').map(|end| at + 5 + end);
                match closing {
                    Some(end) => format!("{}?{query}{}", &line[..end], &line[end..]),
                    None => line.to_owned(),
                }
            } else {
                line.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
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

    /// Asking again helps where a camera is not live yet, or no GPU is
    /// free; not where a channel converts nothing it would need, nor where
    /// something failed.
    #[test]
    fn a_player_who_cannot_play_is_told_whether_to_ask_again() {
        let channel = || "cam1".to_owned();
        let refused = |refusal| PlayError::Refused {
            channel: channel(),
            refusal,
        };
        let cases = [
            (PlayError::NoChannel(channel()), StatusCode::NOT_FOUND, None),
            (
                PlayError::NotLive {
                    channel: channel(),
                    reason: "its camera is not live yet".to_owned(),
                },
                StatusCode::SERVICE_UNAVAILABLE,
                Some(5),
            ),
            (
                PlayError::NotReady { channel: channel() },
                StatusCode::SERVICE_UNAVAILABLE,
                Some(5),
            ),
            (
                refused(Refusal::NoGpu("every session taken".to_owned())),
                StatusCode::SERVICE_UNAVAILABLE,
                Some(30),
            ),
            (
                refused(Refusal::NotConverted {
                    sink: "webrtc",
                    codec: "HEVC".to_owned(),
                }),
                StatusCode::CONFLICT,
                None,
            ),
            (
                refused(Refusal::Failed("no decoder".to_owned())),
                StatusCode::INTERNAL_SERVER_ERROR,
                None,
            ),
        ];
        for (error, status, retry) in cases {
            assert_eq!(answer_to(&error), (status, retry), "{error}");
        }
    }

    /// A playlist asked for with a token in its URL names each file with
    /// the token in its URL too, its initialization section's included.
    #[test]
    fn a_playlist_carries_the_token_it_was_asked_with() {
        let playlist = concat!(
            "#EXTM3U\n",
            r#"#EXT-X-MAP:URI="/recordings/cam1/d/t/media.m4s",BYTERANGE="800@0""#,
            "\n#EXTINF:1.0,\n",
            "#EXT-X-BYTERANGE:4000@800\n",
            "/recordings/cam1/d/t/media.m4s\n",
            "#EXT-X-ENDLIST\n",
        );
        assert_eq!(
            with_token(playlist, "a.b+c"),
            concat!(
                "#EXTM3U\n",
                r#"#EXT-X-MAP:URI="/recordings/cam1/d/t/media.m4s?token=a.b%2Bc",BYTERANGE="800@0""#,
                "\n#EXTINF:1.0,\n",
                "#EXT-X-BYTERANGE:4000@800\n",
                "/recordings/cam1/d/t/media.m4s?token=a.b%2Bc\n",
                "#EXT-X-ENDLIST\n",
            )
        );
    }
}
