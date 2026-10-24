//! Recordings on disk, and what is read back from them: where a channel's
//! go, the playlists its recorder writes, a playlist for a stretch of time
//! across them, and taking away what is older or more than is kept.
//!
//! A recording directory — a chunk — is `<dir>/<channel>/<YYYY-MM-DD>/<HH-MM-SS>/`,
//! its start in UTC, holding an HLS event playlist (`index.m3u8`) whose
//! segments are byte ranges of one fMP4 file (`media.m4s`), the
//! initialization section at its head. Two chunks begun in the same second
//! are `HH-MM-SS` and `HH-MM-SS.1`.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use crate::clock::{Utc, rfc3339};
use crate::config::RecordConfig;

/// The playlist in each chunk.
pub(crate) const PLAYLIST: &str = "index.m3u8";
/// The one media file in each chunk.
pub(crate) const MEDIA: &str = "media.m4s";
/// Where chunks are served from over HTTP: `<URL_ROOT>/<channel>/<date>/<time>/…`.
pub(crate) const URL_ROOT: &str = "/recordings";

/// One recording directory.
#[derive(Debug, Clone)]
pub(crate) struct Chunk {
    pub(crate) path: PathBuf,
    /// When it began, as its directories say.
    pub(crate) start: SystemTime,
    /// Where it is served: `/recordings/<channel>/<date>/<time>`.
    pub(crate) url: String,
}

impl Chunk {
    pub(crate) fn playlist(&self) -> Option<Playlist> {
        fs::read_to_string(self.path.join(PLAYLIST))
            .ok()
            .map(|text| Playlist::parse(&text))
    }

    /// Its files' bytes, and when it was last written.
    pub(crate) fn size_and_written(&self) -> (u64, SystemTime) {
        let mut bytes = 0;
        let mut written = SystemTime::UNIX_EPOCH;
        for entry in fs::read_dir(&self.path).into_iter().flatten().flatten() {
            // The file's own, not its directory entry's: on Windows that
            // says what a file held when it was last closed, and the media
            // file being written is not closed until its recording ends.
            if let Ok(metadata) = fs::metadata(entry.path()) {
                bytes += metadata.len();
                if let Ok(modified) = metadata.modified() {
                    written = written.max(modified);
                }
            }
        }
        (bytes, written)
    }
}

/// Every chunk of the channel called `channel` under `root`, oldest first.
pub(crate) fn chunks(root: &Path, channel: &str) -> Vec<Chunk> {
    let mut chunks = Vec::new();
    let channel_path = root.join(channel);
    for date in dirs(&channel_path) {
        for hms in dirs(&channel_path.join(&date)) {
            let second = hms.split('.').next().unwrap_or_default();
            let Some(start) = Utc::from_dirs(&date, second) else {
                continue;
            };
            chunks.push(Chunk {
                path: channel_path.join(&date).join(&hms),
                start: start.time(),
                url: format!("{URL_ROOT}/{channel}/{date}/{hms}"),
            });
        }
    }
    chunks.sort_by(|a, b| (a.start, &a.path).cmp(&(b.start, &b.path)));
    chunks
}

/// A new chunk's directory for a channel, beginning `now`: made, and
/// returned with what [`chunks`] would say of it.
pub(crate) fn new_chunk(root: &Path, channel: &str, now: SystemTime) -> std::io::Result<Chunk> {
    let utc = Utc::of(now);
    let date_path = root.join(channel).join(utc.date());
    fs::create_dir_all(&date_path)?;
    let mut hms = utc.hms();
    for n in 1.. {
        match fs::create_dir(date_path.join(&hms)) {
            Ok(()) => break,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                hms = format!("{}.{n}", utc.hms());
            }
            Err(error) => return Err(error),
        }
    }
    Ok(Chunk {
        path: date_path.join(&hms),
        start: utc.time(),
        url: format!("{URL_ROOT}/{channel}/{}/{hms}", utc.date()),
    })
}

/// The names of the directories in `path`.
fn dirs(path: &Path) -> Vec<String> {
    fs::read_dir(path)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect()
}

/// What a chunk's playlist says, as far as a playlist across chunks needs.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Playlist {
    /// `#EXT-X-MAP`'s attributes, as written.
    pub(crate) map: Option<String>,
    pub(crate) segments: Vec<Segment>,
    /// Whether it is finished: `#EXT-X-ENDLIST`.
    pub(crate) ended: bool,
}

#[derive(Debug, PartialEq)]
pub(crate) struct Segment {
    /// Seconds.
    pub(crate) duration: f64,
    /// `length@offset`, the offset filled in where the playlist left it to
    /// follow the last.
    pub(crate) range: Option<(u64, u64)>,
    pub(crate) uri: String,
}

impl Playlist {
    pub(crate) fn parse(text: &str) -> Self {
        let mut playlist = Self::default();
        let mut duration = None;
        let mut range = None;
        let mut next_offset = 0;
        for line in text.lines().map(str::trim) {
            if let Some(map) = line.strip_prefix("#EXT-X-MAP:") {
                playlist.map = Some(map.to_owned());
            } else if let Some(extinf) = line.strip_prefix("#EXTINF:") {
                duration = extinf.split(',').next().and_then(|d| d.parse().ok());
            } else if let Some(byterange) = line.strip_prefix("#EXT-X-BYTERANGE:") {
                let (length, offset) = match byterange.split_once('@') {
                    Some((length, offset)) => (length.parse().ok(), offset.parse().ok()),
                    None => (byterange.parse().ok(), Some(next_offset)),
                };
                range = length.zip(offset);
            } else if line == "#EXT-X-ENDLIST" {
                playlist.ended = true;
            } else if !line.is_empty()
                && !line.starts_with('#')
                && let Some(duration) = duration.take()
            {
                if let Some((length, offset)) = range {
                    next_offset = offset + length;
                }
                playlist.segments.push(Segment {
                    duration,
                    range: range.take(),
                    uri: line.to_owned(),
                });
            }
        }
        playlist
    }

    pub(crate) fn duration(&self) -> Duration {
        Duration::from_secs_f64(self.segments.iter().map(|s| s.duration).sum())
    }
}

/// A finished (VOD) playlist of every segment of `chunks` that overlaps
/// `from`..`to`, each chunk after the last with a discontinuity between,
/// and when each chunk's first segment there began. `None` where none
/// does.
///
/// A chunk's start is when its directory was made, and its first segment
/// the keyframe the recording began at, which arrived up to a keyframe
/// interval before: so the times given are late by up to that.
pub(crate) fn playlist_between(
    chunks: &[(Chunk, Playlist)],
    from: SystemTime,
    to: SystemTime,
) -> Option<String> {
    let mut body = String::new();
    let mut longest: f64 = 0.0;
    let mut any = false;
    for (chunk, playlist) in chunks {
        let mut at = chunk.start;
        let mut began = false;
        for segment in &playlist.segments {
            let length = Duration::from_secs_f64(segment.duration);
            let ends = at + length;
            if ends > from && at < to {
                if !began {
                    if any {
                        body.push_str("#EXT-X-DISCONTINUITY\n");
                    }
                    if let Some(map) = &playlist.map {
                        body.push_str(&format!("#EXT-X-MAP:{}\n", absolute_map(map, &chunk.url)));
                    }
                    body.push_str(&format!("#EXT-X-PROGRAM-DATE-TIME:{}\n", rfc3339(at)));
                    began = true;
                    any = true;
                }
                body.push_str(&format!("#EXTINF:{:.6},\n", segment.duration));
                if let Some((length, offset)) = segment.range {
                    body.push_str(&format!("#EXT-X-BYTERANGE:{length}@{offset}\n"));
                }
                body.push_str(&format!("{}/{}\n", chunk.url, segment.uri));
                longest = longest.max(segment.duration);
            }
            at = ends;
        }
    }
    any.then(|| {
        format!(
            "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:{}\n#EXT-X-MEDIA-SEQUENCE:0\n\
             #EXT-X-PLAYLIST-TYPE:VOD\n#EXT-X-INDEPENDENT-SEGMENTS\n{body}#EXT-X-ENDLIST\n",
            longest.ceil() as u64
        )
    })
}

/// `#EXT-X-MAP`'s attributes with its `URI` made absolute under `url`.
fn absolute_map(map: &str, url: &str) -> String {
    match map.split_once("URI=\"") {
        Some((before, after)) => format!("{before}URI=\"{url}/{after}"),
        None => map.to_owned(),
    }
}

/// How often [`keep`] sweeps.
const SWEEP_EVERY: Duration = Duration::from_secs(60);

/// The server's retention: a sweep of the recordings at start and every
/// minute after, on the blocking pool, for as long as the runtime runs —
/// which, where `[record]` keeps everything, removes nothing. What each
/// sweep leaves, in bytes, goes in `measured`, for the metrics.
pub(crate) async fn keep(record: RecordConfig, measured: Arc<Mutex<Option<u64>>>) {
    let retention = Retention {
        keep: record.keep,
        max_size: record.max_size,
        // Written to every segment while it records; a margin over that.
        recording_within: (record.segment * 3).max(Duration::from_secs(60)),
    };
    let mut every = tokio::time::interval(SWEEP_EVERY);
    every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        every.tick().await;
        let root = record.dir.clone();
        let swept =
            tokio::task::spawn_blocking(move || sweep(&root, retention, SystemTime::now())).await;
        if let Ok((removed, left)) = swept {
            if removed > 0 {
                tracing::info!("retention removed {removed} recordings; {left} bytes kept");
            }
            *measured.lock().unwrap_or_else(|p| p.into_inner()) = Some(left);
        }
    }
}

/// What [`sweep`] keeps.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Retention {
    /// How long a chunk is kept from its last write.
    pub(crate) keep: Option<Duration>,
    /// How much every chunk together may take.
    pub(crate) max_size: Option<u64>,
    /// A chunk written to more recently than this, and not finished, is
    /// being recorded: never taken.
    pub(crate) recording_within: Duration,
}

/// Takes away, under `root`, every chunk last written longer ago than
/// `retention.keep`, then the oldest until what is left fits in
/// `retention.max_size`, and the directories that leaves empty. Returns how
/// many chunks went, and the bytes left.
pub(crate) fn sweep(root: &Path, retention: Retention, now: SystemTime) -> (usize, u64) {
    let mut all: Vec<(Chunk, u64, SystemTime, bool)> = Vec::new();
    for channel in dirs(root) {
        for chunk in chunks(root, &channel) {
            let (bytes, written) = chunk.size_and_written();
            let recording = now.duration_since(written).unwrap_or_default()
                < retention.recording_within
                && !chunk.playlist().is_some_and(|playlist| playlist.ended);
            all.push((chunk, bytes, written, recording));
        }
    }
    all.sort_by_key(|(chunk, ..)| chunk.start);
    let mut total: u64 = all.iter().map(|(_, bytes, ..)| bytes).sum();
    let mut removed = 0;
    for (chunk, bytes, written, recording) in &all {
        if *recording {
            continue;
        }
        let expired = retention
            .keep
            .is_some_and(|keep| now.duration_since(*written).unwrap_or_default() > keep);
        let over = retention.max_size.is_some_and(|max| total > max);
        if !(expired || over) {
            continue;
        }
        match fs::remove_dir_all(&chunk.path) {
            Ok(()) => {
                removed += 1;
                total -= bytes;
                tracing::info!(
                    chunk = %chunk.path.display(),
                    "removed: {}",
                    if expired { "older than kept" } else { "over the size kept" }
                );
            }
            Err(error) => {
                tracing::warn!(chunk = %chunk.path.display(), "cannot remove: {error}");
            }
        }
    }
    // Directories a removal left empty: a day's, then a channel's.
    for channel in dirs(root) {
        let channel_path = root.join(&channel);
        for date in dirs(&channel_path) {
            let _ = fs::remove_dir(channel_path.join(date));
        }
        let _ = fs::remove_dir(&channel_path);
    }
    (removed, total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::UNIX_EPOCH;

    const RECORDED: &str = "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:4\n\
        #EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-PLAYLIST-TYPE:EVENT\n\
        #EXT-X-MAP:URI=\"media.m4s\",BYTERANGE=\"764@0\"\n\
        #EXTINF:4.000000,\n#EXT-X-BYTERANGE:1000@764\nmedia.m4s\n\
        #EXTINF:4.000000,\n#EXT-X-BYTERANGE:900\nmedia.m4s\n\
        #EXTINF:2.000000,\n#EXT-X-BYTERANGE:500@2664\nmedia.m4s\n";

    fn at(seconds: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_792_764_000 + seconds) // 2026-10-23T14:00:00Z
    }

    fn scratch(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "relay_pp_{label}_{}_{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_recorded_playlist_is_read_with_every_range_explicit() {
        let playlist = Playlist::parse(RECORDED);
        assert_eq!(
            playlist.map.as_deref(),
            Some("URI=\"media.m4s\",BYTERANGE=\"764@0\"")
        );
        assert!(!playlist.ended);
        let ranges: Vec<_> = playlist.segments.iter().map(|s| s.range).collect();
        assert_eq!(
            ranges,
            [Some((1000, 764)), Some((900, 1764)), Some((500, 2664))]
        );
        assert_eq!(playlist.duration(), Duration::from_secs(10));
        assert!(Playlist::parse(&format!("{RECORDED}#EXT-X-ENDLIST\n")).ended);
    }

    #[test]
    fn a_stretch_across_chunks_takes_the_segments_it_overlaps() {
        let chunk = |start, url: &str| Chunk {
            path: PathBuf::new(),
            start: at(start),
            url: url.to_owned(),
        };
        let chunks = [
            (
                chunk(0, "/recordings/cam1/2026-10-23/14-00-00"),
                Playlist::parse(RECORDED),
            ),
            (
                chunk(60, "/recordings/cam1/2026-10-23/14-01-00"),
                Playlist::parse(RECORDED),
            ),
        ];
        // From the middle of the first chunk's second segment to the start
        // of the second chunk's second one.
        let playlist = playlist_between(&chunks, at(5), at(64) + Duration::from_millis(1)).unwrap();
        let uris: Vec<&str> = playlist.lines().filter(|l| !l.starts_with('#')).collect();
        assert_eq!(uris.len(), 4, "{playlist}");
        assert!(
            playlist.contains("#EXT-X-PLAYLIST-TYPE:VOD") && playlist.ends_with("#EXT-X-ENDLIST\n")
        );
        assert_eq!(
            playlist.matches("#EXT-X-DISCONTINUITY").count(),
            1,
            "{playlist}"
        );
        assert!(playlist.contains(
            "#EXT-X-MAP:URI=\"/recordings/cam1/2026-10-23/14-01-00/media.m4s\",BYTERANGE=\"764@0\""
        ));
        assert!(
            playlist.contains("#EXT-X-PROGRAM-DATE-TIME:2026-10-23T14:00:04.000Z"),
            "{playlist}"
        );
        assert!(playlist.contains("#EXT-X-PROGRAM-DATE-TIME:2026-10-23T14:01:00.000Z"));
        assert!(
            playlist.contains(
                "#EXT-X-BYTERANGE:900@1764\n/recordings/cam1/2026-10-23/14-00-00/media.m4s"
            )
        );
        assert!(
            playlist_between(&chunks, at(20), at(50)).is_none(),
            "between the chunks"
        );
    }

    /// A chunk's media file is held open as it is written: what it holds
    /// so far is counted, though nothing has closed it.
    #[test]
    fn a_chunk_being_written_is_as_large_as_what_it_holds() {
        use std::io::Write;
        let root = scratch("growing");
        let chunk = new_chunk(&root, "cam1", at(0)).unwrap();
        let mut media = fs::File::create(chunk.path.join(MEDIA)).unwrap();
        media.write_all(&[0; 4096]).unwrap();
        media.flush().unwrap();
        let (bytes, _) = chunk.size_and_written();
        assert_eq!(bytes, 4096, "while it is open");
        drop(media);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn chunks_are_named_by_their_start_and_found_again() {
        let root = scratch("chunks");
        let first = new_chunk(&root, "floor2-cam3", at(0)).unwrap();
        let again = new_chunk(&root, "floor2-cam3", at(0)).unwrap();
        let later = new_chunk(&root, "floor2-cam3", at(3600)).unwrap();
        assert!(first.path.ends_with("floor2-cam3/2026-10-23/14-00-00"));
        assert!(again.path.ends_with("floor2-cam3/2026-10-23/14-00-00.1"));
        assert_eq!(later.url, "/recordings/floor2-cam3/2026-10-23/15-00-00");
        let found: Vec<_> = chunks(&root, "floor2-cam3")
            .into_iter()
            .map(|c| c.url)
            .collect();
        assert_eq!(found, [first.url, again.url, later.url]);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_sweep_takes_what_is_old_then_the_oldest_over_size_and_never_what_records() {
        let root = scratch("sweep");
        let write = |chunk: &Chunk, bytes: usize, ended: bool| {
            let end = if ended { "#EXT-X-ENDLIST\n" } else { "" };
            fs::write(chunk.path.join(PLAYLIST), format!("{RECORDED}{end}")).unwrap();
            fs::write(chunk.path.join(MEDIA), vec![0u8; bytes]).unwrap();
        };
        let old = new_chunk(&root, "cam1", at(0)).unwrap();
        let middle = new_chunk(&root, "cam2", at(60)).unwrap();
        let newest = new_chunk(&root, "cam1", at(120)).unwrap();
        let recording = new_chunk(&root, "cam2", at(180)).unwrap();
        for (chunk, ended) in [
            (&old, true),
            (&middle, true),
            (&newest, true),
            (&recording, false),
        ] {
            write(chunk, 10_000, ended);
        }
        let now = SystemTime::now();
        let retention = |keep, max_size| Retention {
            keep,
            max_size,
            recording_within: Duration::from_secs(60),
        };

        // Nothing is old enough yet, and everything fits.
        assert_eq!(
            sweep(&root, retention(Some(Duration::from_secs(3600)), None), now).0,
            0
        );
        // Over the size: the oldest go first until it fits; the one being
        // recorded stays whatever it costs.
        let (removed, left) = sweep(&root, retention(None, Some(25_000)), now);
        assert_eq!(removed, 2);
        assert!(!old.path.exists() && !middle.path.exists());
        assert!(newest.path.exists() && recording.path.exists() && left <= 25_000);
        // Older than kept, from the last write: all but the one recording.
        let (removed, _) = sweep(
            &root,
            retention(Some(Duration::ZERO), None),
            now + Duration::from_secs(1),
        );
        assert_eq!(removed, 1);
        assert!(!newest.path.exists() && recording.path.exists());
        assert!(
            !root.join("cam1").exists(),
            "an emptied channel's directory goes too"
        );
        fs::remove_dir_all(&root).unwrap();
    }
}
