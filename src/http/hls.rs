//! HLS: a channel's live playlist and its files, and a playlist of what it
//! recorded between two times.

use std::collections::HashMap;
use std::time::{Duration, SystemTime};

use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};

use super::{AppState, LIVE_WITHIN, found, unplayable};
use media_pp::elements::{LlHlsPlaylistError, LlHlsPosition};

use crate::channel::{PlayError, is_media_file};
use crate::{clock, recordings};

/// `GET /playback/{name}/index.m3u8?from=…&to=…`: a playlist of what a
/// channel recorded between two RFC 3339 times in UTC — from its first
/// recording, and to now, where either is left out.
pub(super) async fn playback(
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
    let carrying = |uri: &str| {
        let joint = if uri.contains('?') { '&' } else { '?' };
        format!("{uri}{joint}{query}")
    };
    playlist
        .lines()
        .map(|line| {
            if line.is_empty() {
                line.to_owned()
            } else if !line.starts_with('#') {
                carrying(line)
            } else if let Some(at) = line.find("URI=\"") {
                let start = at + 5;
                match line[start..].find('"').map(|end| start + end) {
                    Some(end) => format!(
                        "{}{}{}",
                        &line[..start],
                        carrying(&line[start..end]),
                        &line[end..]
                    ),
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

/// How long a player asking for what is not written yet is held: three
/// segments of a camera that sends a keyframe every two seconds. Past it,
/// it is answered with the playlist as it is.
const BLOCKING_WITHIN: Duration = Duration::from_secs(6);

/// `GET /{name}/index.m3u8`: the channel's live HLS playlist — its HLS
/// written from the first such request, its camera and first part waited
/// for, and written on while a player goes on asking. A player that says
/// where it is — Low-Latency HLS's `_HLS_msn` and `_HLS_part` — is held
/// until what comes after it is written.
pub(super) async fn hls_playlist(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let channel = match found(&state, &name) {
        Ok(channel) => channel,
        Err(refused) => return refused.into_response(),
    };
    // Its players are answered while its HLS is written, the camera live or
    // not, as its RTSP and WebRTC players stay across a drop; only the
    // first waits for the camera and the first segment.
    let stream = match channel.hls_files() {
        Some(stream) => stream,
        None => {
            if let Err(error) = channel.live(LIVE_WITHIN).await {
                return unplayable(&error);
            }
            match channel.hls(LIVE_WITHIN).await {
                Ok(stream) => stream,
                Err(error) => return unplayable(&error),
            }
        }
    };
    let at = match position(&query) {
        Ok(at) => at,
        Err(refused) => return refused.into_response(),
    };
    let held = match at {
        Some(at) => tokio::time::timeout(BLOCKING_WITHIN, stream.playlist(Some(at)))
            .await
            .ok(),
        None => None,
    };
    let playlist = match held {
        Some(answered) => answered,
        // Before a first segment is whole, a player is not held for ever.
        None => tokio::time::timeout(LIVE_WITHIN, stream.playlist(None))
            .await
            .unwrap_or(Err(LlHlsPlaylistError::Closed)),
    };
    let playlist = match playlist {
        Ok(playlist) => playlist,
        Err(LlHlsPlaylistError::TooFarAhead) => {
            return (StatusCode::BAD_REQUEST, "that segment is too far ahead\n").into_response();
        }
        // Ended between the look and the answer: the camera dropped, or
        // nobody asked for a while.
        Err(_) => return unplayable(&PlayError::NotReady { channel: name }),
    };
    (
        [
            (header::CONTENT_TYPE, "application/vnd.apple.mpegurl"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        match (&state.auth, query.get("token")) {
            (Some(_), Some(token)) => with_token(&playlist, token),
            _ => playlist,
        },
    )
        .into_response()
}

/// Where a player says it is — `_HLS_msn`, and `_HLS_part` with it — or
/// the answer refusing what it says.
fn position(
    query: &HashMap<String, String>,
) -> Result<Option<LlHlsPosition>, (StatusCode, &'static str)> {
    let bad = (
        StatusCode::BAD_REQUEST,
        "_HLS_msn and _HLS_part are numbers\n",
    );
    let Some(msn) = query.get("_HLS_msn") else {
        return if query.contains_key("_HLS_part") {
            Err((StatusCode::BAD_REQUEST, "_HLS_part goes with _HLS_msn\n"))
        } else {
            Ok(None)
        };
    };
    let msn = msn.parse().map_err(|_| bad)?;
    let part = match query.get("_HLS_part") {
        Some(part) => Some(part.parse().map_err(|_| bad)?),
        None => None,
    };
    Ok(Some(LlHlsPosition { msn, part }))
}

/// `GET /{name}/{file}`: an init section, a part or a segment of the
/// channel's live HLS, while it is written — the part its playlist says is
/// coming once it is in.
pub(super) async fn hls_file(
    State(state): State<AppState>,
    Path((name, file)): Path<(String, String)>,
) -> Response {
    let stream = state
        .channels
        .get(&name)
        .and_then(|channel| channel.hls_files());
    let (Some(stream), true) = (stream, is_media_file(&file)) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match tokio::time::timeout(BLOCKING_WITHIN, stream.file(&file)).await {
        Ok(Some(bytes)) => (
            [(header::CONTENT_TYPE, "video/mp4")],
            axum::body::Bytes::from(bytes.to_vec()),
        )
            .into_response(),
        // Left the playlist, never coming, or not in time.
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A live playlist names its files as FFmpeg wrote them, beside it.
    #[test]
    fn a_live_playlist_carries_the_token_on_its_relative_names() {
        let playlist = concat!(
            "#EXTM3U\n",
            "#EXT-X-MEDIA-SEQUENCE:4\n",
            r#"#EXT-X-MAP:URI="0-init.mp4""#,
            "\n#EXTINF:1.0,\n",
            "0-00004.m4s\n",
        );
        assert_eq!(
            with_token(playlist, "t"),
            concat!(
                "#EXTM3U\n",
                "#EXT-X-MEDIA-SEQUENCE:4\n",
                r#"#EXT-X-MAP:URI="0-init.mp4?token=t""#,
                "\n#EXTINF:1.0,\n",
                "0-00004.m4s?token=t\n",
            )
        );
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
