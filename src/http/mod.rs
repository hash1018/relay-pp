//! The one HTTP port: the API, WHEP for WebRTC players, HLS live, the
//! player page, recordings, and metrics — see `docs/design.md`, "HTTP".
//! The routes and who may take each are here, and what answers a player
//! who cannot play; what each route does is in the module of its kind:
//! the manager's and the player's API ([`api`]), WebRTC's offers
//! ([`webrtc`]), HLS live and recorded ([`hls`]), and the page ([`page`]).
//!
//! WHEP is answered here over `WebRtcServerHandle::play` and `close` rather
//! than by `WebRtcServer::serve_http`'s listener of its own, so there is one
//! port to open and one place that decides who may play.

mod api;
mod hls;
mod page;
mod webrtc;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use media_pp::elements::{RtspServerHandle, WebRtcServerHandle};
use tower_http::services::ServeDir;

use crate::auth::{self, Auth, FromHeader, Presented, Right};
use crate::channel::{AttachError, Channel, Output, PlayError, Refusal};
use crate::channels::Channels;
use crate::recordings;
use api::{
    create_channel, delete_channel, list_channels, list_recordings, metrics, one_channel, play,
    record, unrecord,
};
use hls::{hls_file, hls_playlist, playback};
use page::{hls_js, player_page};
use webrtc::{whep_end, whep_offer, whep_options, whip_offer, whip_options};

/// How long a player's request waits for a channel's camera to be live —
/// one just made, or connecting again — before it is answered that it is
/// not.
const LIVE_WITHIN: Duration = Duration::from_secs(10);

/// What the routes reach.
#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) channels: Arc<Channels>,
    pub(crate) webrtc: WebRtcServerHandle,
    /// The RTSP listener, for how many play.
    pub(crate) rtsp: RtspServerHandle,
    /// The port RTSP players connect to.
    pub(crate) rtsp_port: u16,
    /// What RTSP URLs begin with in place of the host a request came to
    /// and `rtsp_port`.
    pub(crate) rtsp_public_url: Option<String>,
    /// Who may do what; anybody anything, where `None`.
    pub(crate) auth: Option<Arc<Auth>>,
    /// What the recordings take on disk, as the retention sweep last
    /// measured it.
    pub(crate) recorded_bytes: Arc<Mutex<Option<u64>>>,
    /// Whether the page that plays a channel is served (`[http]
    /// player_page`).
    pub(crate) player_page: bool,
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
        .route("/metrics", get(metrics))
        .route_layer(middleware::from_fn_with_state(state.clone(), manager_only));
    let players = Router::new()
        .route("/api/v1/channels/{name}/play", post(play))
        .route("/{name}/whep", post(whep_offer).options(whep_options))
        .route("/{name}/index.m3u8", get(hls_playlist))
        .route("/{name}/{file}", get(hls_file))
        .route_layer(middleware::from_fn_with_state(
            (state.clone(), Right::Play),
            who_may,
        ));
    let publishers = Router::new()
        .route("/{name}/whip", post(whip_offer).options(whip_options))
        .route_layer(middleware::from_fn_with_state(
            (state.clone(), Right::Publish),
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
    let mut router = Router::new()
        .merge(manager)
        .merge(players)
        .merge(publishers)
        .merge(watchers)
        .nest(recordings::URL_ROOT, files)
        // The session's own URL is the secret that ends it.
        .route("/{name}/whep/{session}", axum::routing::delete(whep_end))
        .route("/{name}/whip/{session}", axum::routing::delete(whep_end));
    // The page and what it runs carry nothing of a channel: what it plays,
    // it asks for with the token it was opened with. Where players are of
    // their own, it is not served at all.
    if state.player_page {
        router = router
            .route("/{name}", get(player_page))
            .route("/assets/hls.light.min.js", get(hls_js));
    }
    router.with_state(state)
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
        // Begun again in a moment.
        PlayError::Hls { .. } => (StatusCode::SERVICE_UNAVAILABLE, Some(2)),
    }
}

fn attach_refusal(error: &AttachError) -> (StatusCode, String) {
    let status = match error {
        AttachError::NoChannel(_) | AttachError::NotAttached(_) => StatusCode::NOT_FOUND,
        AttachError::Attached(_) => StatusCode::CONFLICT,
        AttachError::Listener { .. } => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (status, format!("{error}\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
