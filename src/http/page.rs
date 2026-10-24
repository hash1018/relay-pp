//! The page that plays a channel in a browser, and what it runs.

use axum::http::header;
use axum::response::{IntoResponse, Response};

/// The page that plays a channel in a browser: WebRTC, HLS where WebRTC
/// does not get through, and its recordings.
const PLAYER_PAGE: &str = include_str!("../../assets/player.html");

/// hls.js 1.7.3, its light build, as released (`assets/hls.js/LICENSE`):
/// the page plays HLS with it in a browser that plays none itself, on a
/// network that reaches no CDN.
const HLS_JS: &[u8] = include_bytes!("../../assets/hls.js/hls.light.min.js");

/// `GET /{name}`: the page that plays a channel.
pub(super) async fn player_page() -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        PLAYER_PAGE,
    )
        .into_response()
}

/// `GET /assets/hls.light.min.js`.
pub(super) async fn hls_js() -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        HLS_JS,
    )
        .into_response()
}
